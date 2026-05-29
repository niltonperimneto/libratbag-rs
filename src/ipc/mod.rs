/* DBus surface: zbus interface implementations for Manager/Device/Profile/Resolution/Button/LED,
 * plus helpers to register devices and translate device actions from udev. */
pub mod button;
pub mod device;
pub mod led;
pub mod manager;
pub mod profile;
pub mod resolution;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};
use zbus::connection::Builder;
use zbus::zvariant::OwnedValue;

use crate::engine::actor::{self, ActorHandle};
use crate::engine::device::DeviceInfo;
use crate::engine::device_database::{BusType, DeviceDb};
use crate::hal;
use crate::udev_monitor::DeviceAction;

/// Fallback [`OwnedValue`] (`u32` zero) used when zvariant serialization fails.
#[inline]
pub(crate) fn fallback_owned_value() -> OwnedValue {
    OwnedValue::from(0u32)
}

/* Walk an error chain looking for an `EACCES` (permission denied) cause.
 *
 * Under the unprivileged session-daemon model the device node is opened via
 * the `uaccess` ACL that systemd-logind grants to the seated user. On hotplug
 * the node appears slightly before logind applies that ACL, so the first
 * open() can transiently fail with `Permission denied`. We treat that case as
 * retryable (with a longer back-off) rather than a hard failure. */
fn is_permission_denied(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

/* D-Bus interface tag stored alongside each object path so that teardown
 * removes only the correct interface type in O(n) rather than blindly
 * attempting all five types per path. */
#[derive(Debug, Clone, Copy)]
enum IfaceKind {
    Device,
    Profile,
    Resolution,
    Button,
    Led,
}

/* Register a new device and its children (profiles, buttons, etc) onto the
 * D-Bus bus.
 *
 * Returns a tagged list of all object paths that were registered.  Child
 * objects share the same `Arc<RwLock<DeviceInfo>>` so property mutations
 * propagate to the device-level `commit()` path. */
async fn register_device_on_dbus(
    conn: &zbus::Connection,
    device_path: &str,
    shared_info: Arc<RwLock<DeviceInfo>>,
    actor_handle: Option<ActorHandle>,
) -> Vec<(String, IfaceKind)> {
    let mut object_paths: Vec<(String, IfaceKind)> = Vec::with_capacity(64);
    let object_server = conn.object_server();

    /* Register the Device object. */
    let device_obj = device::RatbagDevice::new(
        Arc::clone(&shared_info),
        device_path.to_owned(),
        actor_handle,
    );

    if let Err(e) = object_server.at(device_path, device_obj).await {
        warn!("Failed to register device at {device_path}: {e}");
        object_paths.push((device_path.to_owned(), IfaceKind::Device));
        return object_paths;
    }
    object_paths.push((device_path.to_owned(), IfaceKind::Device));

    /* Register Profile, Resolution, Button, LED child objects.
     * We snapshot the structure for iteration but children hold the shared
     * Arc so mutations propagate correctly to the commit path. */
    let info_snapshot = shared_info.read().await;
    for prof in &info_snapshot.profiles {
        let profile_path = format!("{device_path}/p{}", prof.index);
        let profile_obj = profile::RatbagProfile::new(
            Arc::clone(&shared_info),
            device_path.to_owned(),
            prof.index,
        );
        if let Err(e) = object_server.at(profile_path.as_str(), profile_obj).await {
            warn!("Failed to register profile {profile_path}: {e}");
        }
        object_paths.push((profile_path.clone(), IfaceKind::Profile));

        for res in &prof.resolutions {
            let res_path = format!("{device_path}/p{}/r{}", prof.index, res.index);
            let res_obj = resolution::RatbagResolution::new(
                Arc::clone(&shared_info),
                device_path.to_owned(),
                prof.index,
                res.index,
            );
            if let Err(e) = object_server.at(res_path.as_str(), res_obj).await {
                warn!("Failed to register resolution {res_path}: {e}");
            }
            object_paths.push((res_path, IfaceKind::Resolution));
        }

        for btn in &prof.buttons {
            let btn_path = format!("{device_path}/p{}/b{}", prof.index, btn.index);
            let btn_obj = button::RatbagButton::new(
                Arc::clone(&shared_info),
                prof.index,
                btn.index,
            );
            if let Err(e) = object_server.at(btn_path.as_str(), btn_obj).await {
                warn!("Failed to register button {btn_path}: {e}");
            }
            object_paths.push((btn_path, IfaceKind::Button));
        }

        for led_info in &prof.leds {
            let led_path = format!("{device_path}/p{}/l{}", prof.index, led_info.index);
            let led_obj = led::RatbagLed::new(
                Arc::clone(&shared_info),
                prof.index,
                led_info.index,
            );
            if let Err(e) = object_server.at(led_path.as_str(), led_obj).await {
                warn!("Failed to register LED {led_path}: {e}");
            }
            object_paths.push((led_path, IfaceKind::Led));
        }
    }

    object_paths
}

/* Unregister a device and all its children from the D-Bus object server,
 * then remove it from the manager's device list.
 *
 * Shared between the `Remove` (udev) and `RemoveTest` (dev-hooks) paths. */
async fn remove_device(
    conn: &zbus::Connection,
    sysname: &str,
    registered_devices: &mut HashMap<String, Vec<(String, IfaceKind)>>,
    actor_handles: &mut HashMap<String, ActorHandle>,
) -> Result<()> {
    /* Shut down the hardware actor if one is running. */
    if let Some(handle) = actor_handles.remove(sysname) {
        handle.shutdown().await;
    }

    if let Some(paths) = registered_devices.remove(sysname) {
        let object_server = conn.object_server();

        /* Remove child objects first (reverse order), then the device itself.
         * Each path is tagged with its interface type so we issue exactly one
         * removal call per path instead of blindly trying all five. */
        for (path, kind) in paths.iter().rev() {
            let result = match kind {
                IfaceKind::Device =>
                    object_server.remove::<device::RatbagDevice, _>(path.as_str()).await,
                IfaceKind::Profile =>
                    object_server.remove::<profile::RatbagProfile, _>(path.as_str()).await,
                IfaceKind::Resolution =>
                    object_server.remove::<resolution::RatbagResolution, _>(path.as_str()).await,
                IfaceKind::Button =>
                    object_server.remove::<button::RatbagButton, _>(path.as_str()).await,
                IfaceKind::Led =>
                    object_server.remove::<led::RatbagLed, _>(path.as_str()).await,
            };
            match result {
                Ok(false) | Err(_) => {
                    warn!("Failed to remove {:?} object at {}", kind, path);
                }
                Ok(true) => {}
            }
        }

        /* The device root path is always paths[0]; update the manager list. */
        let device_path = &paths[0].0;
        let iface_ref = object_server
            .interface::<_, manager::RatbagManager>("/org/freedesktop/ratbag1")
            .await?;
        iface_ref.get_mut().await.remove_device(device_path);
        iface_ref
            .get()
            .await
            .devices_changed(iface_ref.signal_emitter())
            .await?;

        info!("Device {} removed ({} objects)", sysname, paths.len());
    } else {
        info!("Device removed: {} (was not registered)", sysname);
    }

    Ok(())
}

/// Start the DBus server and register all interfaces.
///
/// This function blocks until the daemon is shut down. It receives device
/// hotplug events from the udev monitor through the `device_rx` channel.
pub async fn run_server(
    mut device_rx: mpsc::Receiver<DeviceAction>,
    device_db: DeviceDb,
) -> Result<()> {
    let manager = manager::RatbagManager::default();

    let conn = Builder::session()?
        .name("org.freedesktop.ratbag1")?
        .serve_at("/org/freedesktop/ratbag1", manager)?
        .build()
        .await?;

    info!("DBus server ready on org.freedesktop.ratbag1");

    // Under dev-hooks, wire a secondary channel to the manager so that
    // LoadTestDevice / ResetTestDevice can inject synthetic DeviceActions
    // into this same event loop.
    #[cfg(feature = "dev-hooks")]
    let mut test_rx = {
        let (test_tx, test_rx) =
            tokio::sync::mpsc::channel::<DeviceAction>(16);
        let object_server = conn.object_server();
        let iface_ref = object_server
            .interface::<_, manager::RatbagManager>("/org/freedesktop/ratbag1")
            .await?;
        iface_ref.get_mut().await.set_test_device_tx(test_tx);
        test_rx
    };

    /* Track registered device paths so we can clean up on removal. */
    let mut registered_devices: HashMap<String, Vec<(String, IfaceKind)>> = HashMap::new();

    // Track actor handles so we can shut them down on removal.
    let mut actor_handles: HashMap<String, ActorHandle> = HashMap::new();

    /* Track which physical devices already have a successfully probed
     * device registered.  HID++ mice (and some other devices) expose
     * multiple hidraw nodes per physical device — typically one for the
     * standard HID interface and one for the vendor-specific command
     * channel.  Both nodes share the same USB topology path (the
     * HID_PHYS property with the /inputN suffix stripped).
     *
     * The dedup key is `"{phys_path}\0{hid_uniq}"`.  Using phys_path
     * alone would incorrectly collapse two mice paired to the same
     * multi-device receiver (Logitech Unifying, Bolt, etc.) since they
     * share the same USB topology.  Including HID_UNIQ (the per-device
     * serial number) distinguishes them while still deduplicating the
     * multiple hidraw nodes of a single mouse (which share both
     * phys_path AND HID_UNIQ).
     *
     * `sysname_to_dedup_key` maps each registered sysname back to its
     * dedup key so that the Remove handler can clear the entry from
     * `probed_devices`, allowing re-probing on hotplug. */
    let mut probed_devices: HashSet<String> = HashSet::new();
    let mut sysname_to_dedup_key: HashMap<String, String> = HashMap::new();

    // Main event loop: process udev device events (and, when dev-hooks is
    // enabled, synthetic test device actions from the DBus manager).
    loop {
        // Multiplex the udev channel with the optional test channel.
        #[cfg(feature = "dev-hooks")]
        let action = tokio::select! {
            a = device_rx.recv() => match a { Some(a) => a, None => break },
            a = test_rx.recv()   => match a { Some(a) => a, None => break },
        };
        #[cfg(not(feature = "dev-hooks"))]
        let action = match device_rx.recv().await {
            Some(a) => a,
            None => break,
        };

        match action {
            DeviceAction::Add {
                sysname,
                devnode,
                name,
                bustype,
                vid,
                pid,
                phys_path,
                hid_uniq,
            } => {
                let db_key = (BusType::from_u16(bustype), vid, pid);

                let entry = match device_db.get(&db_key) {
                    Some(e) => e,
                    None => {
                        info!(
                            "Ignoring unsupported device {} ({:04x}:{:04x})",
                            sysname, vid, pid
                        );
                        continue;
                    }
                };

                /* A single physical device (e.g. Logitech G403 HERO) can
                 * expose multiple hidraw nodes — one for the standard HID
                 * mouse interface and one for the vendor-specific HID++
                 * command channel.  Both nodes share the same USB topology
                 * path (phys_path) AND the same serial (hid_uniq).
                 *
                 * The dedup key combines both fields so that:
                 * - Multiple hidraw nodes of ONE mouse are collapsed (same
                 *   phys + same uniq).
                 * - Two mice on the same multi-device receiver (Unifying,
                 *   Bolt) are kept separate (same phys, different uniq).
                 * - Two identical mice on different USB ports are kept
                 *   separate (different phys). */
                let dedup_key = format!("{}\0{}", phys_path, hid_uniq);
                if !phys_path.is_empty() && probed_devices.contains(&dedup_key) {
                    info!(
                        "Skipping {} ({:04x}:{:04x}): already probed on another hidraw node \
                         (phys={}, uniq={})",
                        sysname, vid, pid, phys_path, hid_uniq
                    );
                    continue;
                }

                info!(
                    "Matched device: {} -> {} (driver: {})",
                    sysname, entry.name, entry.driver
                );

                let device_info =
                    DeviceInfo::from_entry(&sysname, &name, bustype, vid, pid, entry);
                let device_path = format!(
                    "/org/freedesktop/ratbag1/device/{}",
                    sysname.replace('-', "_")
                );

                // Wrap DeviceInfo in Arc<RwLock> so actor and DBus share state.
                // `mut` because each probe attempt installs fresh state.
                let mut shared_info = Arc::new(RwLock::new(device_info));

                /* Try to create and spawn the hardware driver actor, retrying
                 * a few times with a back-off. If every attempt fails (wrong
                 * hidraw interface, unsupported firmware, etc.) do NOT register
                 * the device on D-Bus — it would appear as an empty,
                 * non-functional entry.
                 *
                 * Retries cover two distinct transient conditions:
                 *   - USB settle: the device node exists but the hardware is
                 *     not ready to answer probe requests yet.
                 *   - uaccess ACL race: on hotplug the node appears slightly
                 *     before systemd-logind grants the seated user access, so
                 *     open() fails with EACCES for a brief window. See
                 *     is_permission_denied(). */
                const MAX_PROBE_ATTEMPTS: u32 = 5;
                const BASE_SETTLE_DELAY: Duration = Duration::from_millis(500);

                let mut actor_handle = None;
                for attempt in 1..=MAX_PROBE_ATTEMPTS {
                    let Some(drv) = hal::create_driver(&entry.driver) else {
                        warn!(
                            "No driver implementation for '{}', skipping {}",
                            entry.driver, sysname
                        );
                        break;
                    };

                    /* Fresh state each attempt: a failed probe may have
                     * partially mutated it. */
                    let attempt_info = Arc::new(RwLock::new(DeviceInfo::from_entry(
                        &sysname, &name, bustype, vid, pid, entry,
                    )));

                    match actor::spawn_device_actor(&devnode, drv, Arc::clone(&attempt_info))
                        .await
                    {
                        Ok(handle) => {
                            shared_info = attempt_info;
                            if attempt > 1 {
                                info!(
                                    "Driver {} active for {} (retry {attempt} succeeded)",
                                    entry.driver, sysname
                                );
                            } else {
                                info!("Driver {} active for {}", entry.driver, sysname);
                            }
                            actor_handle = Some(handle);
                            break;
                        }
                        Err(e) => {
                            if attempt == MAX_PROBE_ATTEMPTS {
                                warn!(
                                    "Driver {} probe failed for {} \
                                     (attempt {attempt}/{MAX_PROBE_ATTEMPTS}): {e:#}",
                                    entry.driver, sysname
                                );
                                break;
                            }

                            /* Back off longer for EACCES: the uaccess ACL may
                             * still be on its way from logind. */
                            let delay = if is_permission_denied(&e) {
                                BASE_SETTLE_DELAY * attempt
                            } else {
                                BASE_SETTLE_DELAY
                            };
                            info!(
                                "Driver {} probe failed for {} \
                                 (attempt {attempt}/{MAX_PROBE_ATTEMPTS}): {e:#}, \
                                 retrying in {delay:?}",
                                entry.driver, sysname
                            );
                            tokio::time::sleep(delay).await;
                        }
                    }
                }

                let Some(actor_handle) = actor_handle else {
                    continue;
                };

                let object_paths = register_device_on_dbus(
                    &conn,
                    &device_path,
                    Arc::clone(&shared_info),
                    Some(actor_handle.clone()),
                )
                .await;

                let child_count = object_paths.len().saturating_sub(1);

                /* Update the manager's device list.  Errors here are
                 * non-fatal — the device actor is already running and
                 * the D-Bus objects are registered; only the manager's
                 * aggregated Devices list would be stale. */
                let manager_ok = async {
                    let object_server = conn.object_server();
                    let iface_ref = object_server
                        .interface::<_, manager::RatbagManager>(
                            "/org/freedesktop/ratbag1",
                        )
                        .await?;
                    iface_ref.get_mut().await.add_device(device_path.clone());
                    iface_ref
                        .get()
                        .await
                        .devices_changed(iface_ref.signal_emitter())
                        .await?;
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if let Err(e) = manager_ok {
                    warn!("Failed to update manager device list for {}: {e:#}", sysname);
                }

                actor_handles.insert(sysname.clone(), actor_handle);
                registered_devices.insert(sysname.clone(), object_paths);
                if !phys_path.is_empty() {
                    probed_devices.insert(dedup_key.clone());
                    sysname_to_dedup_key.insert(sysname.clone(), dedup_key);
                }

                info!(
                    "Device {} registered at {} ({} child objects)",
                    entry.name, device_path, child_count
                );
            }

            DeviceAction::Remove { sysname } => {
                /* Clear the probed-device entry so a re-plugged device
                 * can be discovered again on a fresh hidraw node. */
                if let Some(key) = sysname_to_dedup_key.remove(&sysname) {
                    probed_devices.remove(&key);
                }
                if let Err(e) = remove_device(
                    &conn,
                    &sysname,
                    &mut registered_devices,
                    &mut actor_handles,
                )
                .await
                {
                    warn!("Failed to cleanly remove device {}: {e:#}", sysname);
                }
            }

            // ----------------------------------------------------------------
            // dev-hooks only: synthetic test device injection
            // ----------------------------------------------------------------
            #[cfg(feature = "dev-hooks")]
            DeviceAction::InjectTest { sysname, device_info } => {
                let device_path = format!(
                    "/org/freedesktop/ratbag1/device/{}",
                    sysname.replace('-', "_")
                );

                info!("InjectTest: registering '{}' at {}", sysname, device_path);

                let shared_info = Arc::new(RwLock::new(device_info));

                /* Test devices have no hardware actor. */
                let object_paths = register_device_on_dbus(
                    &conn,
                    &device_path,
                    Arc::clone(&shared_info),
                    None,
                )
                .await;

                let manager_ok = async {
                    let object_server = conn.object_server();
                    let iface_ref = object_server
                        .interface::<_, manager::RatbagManager>(
                            "/org/freedesktop/ratbag1",
                        )
                        .await?;
                    iface_ref.get_mut().await.add_device(device_path.clone());
                    iface_ref
                        .get()
                        .await
                        .devices_changed(iface_ref.signal_emitter())
                        .await?;
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if let Err(e) = manager_ok {
                    warn!("Failed to update manager for test device {}: {e:#}", sysname);
                }

                registered_devices.insert(sysname, object_paths);
            }

            #[cfg(feature = "dev-hooks")]
            DeviceAction::RemoveTest { sysname } => {
                if let Err(e) = remove_device(
                    &conn,
                    &sysname,
                    &mut registered_devices,
                    &mut actor_handles,
                )
                .await
                {
                    warn!("Failed to remove test device {}: {e:#}", sysname);
                }
            }
        }
    }

    info!("udev monitor channel closed, shutting down");
    Ok(())
}
