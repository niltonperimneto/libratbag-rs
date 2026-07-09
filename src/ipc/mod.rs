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

/* Number of probe attempts for a freshly-added device.  Retries cover
 * USB settle time and the logind uaccess ACL race (see the comments in
 * `probe_and_register`). */
const MAX_PROBE_ATTEMPTS: u32 = 5;

/* Number of probe attempts for a wake-triggered re-probe of a parked
 * device.  Lighter than the initial burst: if the device is still not
 * answering it simply gets parked again. */
const REPROBE_ATTEMPTS: u32 = 2;

/* Base back-off between probe attempts. */
const BASE_SETTLE_DELAY: Duration = Duration::from_millis(500);

/* Identity of a hidraw node as reported by udev, carried through the
 * probe/park/re-probe lifecycle. */
#[derive(Debug, Clone)]
struct HidrawDevice {
    sysname: String,
    devnode: std::path::PathBuf,
    name: String,
    bustype: u16,
    vid: u16,
    pid: u16,
    phys_path: String,
    hid_uniq: String,
}

impl HidrawDevice {
    /* Deduplication key: multiple hidraw nodes of ONE physical device
     * share phys_path AND hid_uniq; two mice on the same multi-device
     * receiver differ in hid_uniq.  Only meaningful when phys_path is
     * non-empty. */
    fn dedup_key(&self) -> String {
        format!("{}\0{}", self.phys_path, self.hid_uniq)
    }
}

/* A DB-matched device whose probe failed (typically a wireless mouse
 * that is asleep or powered off).  A wake-watcher task holds the node
 * open and triggers a re-probe as soon as the device emits any report,
 * with a periodic timer as fallback. */
struct PendingDevice {
    dev: HidrawDevice,
    dedup_key: String,
    /* Completed park/re-probe cycles; scales the fallback timer. */
    attempts: u32,
    watcher: tokio::task::JoinHandle<()>,
}

/* Mutable bookkeeping owned by the run_server event loop. */
#[derive(Default)]
struct ServerState {
    /* Registered device paths, for teardown on removal. */
    registered_devices: HashMap<String, Vec<(String, IfaceKind)>>,
    /* Actor handles, for shutdown on removal. */
    actor_handles: HashMap<String, ActorHandle>,
    /* Dedup keys of successfully probed physical devices. */
    probed_devices: HashSet<String>,
    /* sysname → dedup key, so Remove can clear `probed_devices`. */
    sysname_to_dedup_key: HashMap<String, String>,
    /* Probe-failed devices awaiting a wake-triggered re-probe. */
    pending_devices: HashMap<String, PendingDevice>,
}

/* Outcome of `probe_and_register` for one hidraw node. */
enum AddOutcome {
    Registered,
    ProbeFailed,
    NoDriver,
}

/* Walk an error chain looking for DriverError::DeviceAsleep: the
 * receiver answered that the paired device is unreachable right now. */
fn is_device_asleep(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<hal::DriverError>(),
            Some(hal::DriverError::DeviceAsleep)
        )
    })
}

/* Watch a parked device's hidraw node and fire a re-probe request when
 * it shows signs of life.
 *
 * hidraw supports multiple concurrent opens with per-fd report queues,
 * so this passive watcher (zero writes) cannot interfere with anything.
 * ANY queued report — even plain mouse motion after the user touches
 * the mouse — trips readiness, giving near-instant registration on
 * wake.  If the node cannot be opened or watched, a timer provides the
 * fallback cadence (30 s for the first cycles, then 60 s).
 *
 * The task always ends by sending on `tx`, which also guarantees the
 * watcher's fd is closed before the main loop re-opens the node for
 * probing. */
fn spawn_wake_watcher(
    devnode: std::path::PathBuf,
    sysname: String,
    tx: mpsc::Sender<String>,
    attempts: u32,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let fallback = Duration::from_secs(if attempts < 2 { 30 } else { 60 });
        match hal::DeviceIo::open(&devnode).await {
            Ok(io) => {
                tokio::select! {
                    result = io.wait_readable() => match result {
                        Ok(()) => {
                            info!("{sysname}: parked device emitted a report; re-probing");
                        }
                        Err(e) => {
                            warn!("{sysname}: wake watch failed ({e}); waiting {fallback:?}");
                            tokio::time::sleep(fallback).await;
                        }
                    },
                    _ = tokio::time::sleep(fallback) => {
                        info!("{sysname}: periodic re-probe timer fired");
                    }
                }
            }
            Err(e) => {
                /* EACCES (uaccess ACL not yet applied) or a vanished node:
                 * fall back to the timer; a re-plug also cancels us. */
                warn!("{sysname}: cannot watch node for wake-up ({e:#}); waiting {fallback:?}");
                tokio::time::sleep(fallback).await;
            }
        }
        /* The DeviceIo is dropped before this send, so the re-probe never
         * races the watcher for the node. */
        let _ = tx.send(sysname).await;
    })
}

/* Park a probe-failed device: remember it and spawn its wake-watcher. */
fn park_device(
    state: &mut ServerState,
    dev: HidrawDevice,
    attempts: u32,
    reprobe_tx: &mpsc::Sender<String>,
) {
    let watcher = spawn_wake_watcher(
        dev.devnode.clone(),
        dev.sysname.clone(),
        reprobe_tx.clone(),
        attempts,
    );
    info!(
        "Parked {} pending wake-up or periodic re-probe (cycle {})",
        dev.sysname,
        attempts + 1
    );
    let dedup_key = dev.dedup_key();
    state.pending_devices.insert(
        dev.sysname.clone(),
        PendingDevice {
            dev,
            dedup_key,
            attempts,
            watcher,
        },
    );
}

/* Try to probe the hardware and, on success, register the device tree
 * on D-Bus and update all bookkeeping.
 *
 * Retries cover two distinct transient conditions:
 *   - USB settle: the device node exists but the hardware is not ready
 *     to answer probe requests yet.
 *   - uaccess ACL race: on hotplug the node appears slightly before
 *     systemd-logind grants the seated user access, so open() fails
 *     with EACCES for a brief window (see is_permission_denied()).
 *
 * A DeviceAsleep probe result aborts the retry burst immediately — the
 * device will not answer until it wakes, so the caller parks it for a
 * wake-triggered re-probe instead. */
async fn probe_and_register(
    conn: &zbus::Connection,
    entry: &crate::engine::device_database::DeviceEntry,
    dev: &HidrawDevice,
    state: &mut ServerState,
    max_attempts: u32,
) -> AddOutcome {
    let device_path = format!(
        "/org/freedesktop/ratbag1/device/{}",
        dev.sysname.replace('-', "_")
    );

    let mut registered: Option<(ActorHandle, Arc<RwLock<DeviceInfo>>)> = None;
    for attempt in 1..=max_attempts {
        let Some(drv) = hal::create_driver(&entry.driver) else {
            warn!(
                "No driver implementation for '{}', skipping {}",
                entry.driver, dev.sysname
            );
            return AddOutcome::NoDriver;
        };

        /* Fresh state each attempt: a failed probe may have partially
         * mutated it. */
        let attempt_info = Arc::new(RwLock::new(DeviceInfo::from_entry(
            &dev.sysname,
            &dev.name,
            dev.bustype,
            dev.vid,
            dev.pid,
            entry,
        )));

        match actor::spawn_device_actor(&dev.devnode, drv, Arc::clone(&attempt_info)).await {
            Ok(handle) => {
                if attempt > 1 {
                    info!(
                        "Driver {} active for {} (retry {attempt} succeeded)",
                        entry.driver, dev.sysname
                    );
                } else {
                    info!("Driver {} active for {}", entry.driver, dev.sysname);
                }
                registered = Some((handle, attempt_info));
                break;
            }
            Err(e) => {
                if is_device_asleep(&e) {
                    info!(
                        "Driver {} reports {} is asleep or powered off; deferring",
                        entry.driver, dev.sysname
                    );
                    return AddOutcome::ProbeFailed;
                }
                if attempt == max_attempts {
                    warn!(
                        "Driver {} probe failed for {} \
                         (attempt {attempt}/{max_attempts}): {e:#}",
                        entry.driver, dev.sysname
                    );
                    return AddOutcome::ProbeFailed;
                }

                /* Back off longer for EACCES: the uaccess ACL may still
                 * be on its way from logind. */
                let delay = if is_permission_denied(&e) {
                    BASE_SETTLE_DELAY * attempt
                } else {
                    BASE_SETTLE_DELAY
                };
                info!(
                    "Driver {} probe failed for {} \
                     (attempt {attempt}/{max_attempts}): {e:#}, retrying in {delay:?}",
                    entry.driver, dev.sysname
                );
                tokio::time::sleep(delay).await;
            }
        }
    }

    let Some((actor_handle, shared_info)) = registered else {
        return AddOutcome::ProbeFailed;
    };

    let object_paths = register_device_on_dbus(
        conn,
        &device_path,
        Arc::clone(&shared_info),
        Some(actor_handle.clone()),
    )
    .await;

    let child_count = object_paths.len().saturating_sub(1);

    /* Update the manager's device list.  Errors here are non-fatal —
     * the device actor is already running and the D-Bus objects are
     * registered; only the manager's aggregated Devices list would be
     * stale. */
    let manager_ok = async {
        let object_server = conn.object_server();
        let iface_ref = object_server
            .interface::<_, manager::RatbagManager>("/org/freedesktop/ratbag1")
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
        warn!(
            "Failed to update manager device list for {}: {e:#}",
            dev.sysname
        );
    }

    state
        .actor_handles
        .insert(dev.sysname.clone(), actor_handle);
    state
        .registered_devices
        .insert(dev.sysname.clone(), object_paths);
    if !dev.phys_path.is_empty() {
        let dedup_key = dev.dedup_key();
        state.probed_devices.insert(dedup_key.clone());
        state
            .sysname_to_dedup_key
            .insert(dev.sysname.clone(), dedup_key);
    }

    info!(
        "Device {} registered at {} ({} child objects)",
        entry.name, device_path, child_count
    );

    AddOutcome::Registered
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

    /* All mutable bookkeeping for the event loop; see ServerState. */
    let mut state = ServerState::default();

    /* Internal channel through which wake-watchers request a re-probe
     * of a parked device.  `reprobe_tx` is kept alive here so the recv
     * arm below simply pends (rather than closing) when no watchers
     * are running. */
    let (reprobe_tx, mut reprobe_rx) = mpsc::channel::<String>(16);

    /* Event source for one loop iteration. */
    enum LoopEvent {
        Action(DeviceAction),
        Reprobe(String),
    }

    // Main event loop: process udev device events, internal re-probe
    // requests (and, when dev-hooks is enabled, synthetic test device
    // actions from the DBus manager).
    loop {
        #[cfg(feature = "dev-hooks")]
        let event = tokio::select! {
            a = device_rx.recv() => match a { Some(a) => LoopEvent::Action(a), None => break },
            a = test_rx.recv()   => match a { Some(a) => LoopEvent::Action(a), None => break },
            Some(s) = reprobe_rx.recv() => LoopEvent::Reprobe(s),
        };
        #[cfg(not(feature = "dev-hooks"))]
        let event = tokio::select! {
            a = device_rx.recv() => match a { Some(a) => LoopEvent::Action(a), None => break },
            Some(s) = reprobe_rx.recv() => LoopEvent::Reprobe(s),
        };

        let action = match event {
            LoopEvent::Reprobe(sysname) => {
                /* Removed (unplugged) while the message was in flight? */
                let Some(pending) = state.pending_devices.remove(&sysname) else {
                    continue;
                };

                /* Another hidraw node of the same physical device may have
                 * registered while we were parked. */
                if !pending.dev.phys_path.is_empty()
                    && state.probed_devices.contains(&pending.dedup_key)
                {
                    info!(
                        "Skipping re-probe of {}: already registered via another node",
                        sysname
                    );
                    continue;
                }

                let db_key = (
                    BusType::from_u16(pending.dev.bustype),
                    pending.dev.vid,
                    pending.dev.pid,
                );
                let Some(entry) = device_db.get(&db_key) else {
                    continue;
                };

                info!("Re-probing parked device {} ({})", sysname, entry.name);
                match probe_and_register(
                    &conn,
                    entry,
                    &pending.dev,
                    &mut state,
                    REPROBE_ATTEMPTS,
                )
                .await
                {
                    AddOutcome::Registered => {
                        /* Sibling hidraw nodes of the same physical device
                         * no longer need their own re-probes. */
                        if !pending.dev.phys_path.is_empty() {
                            let siblings: Vec<String> = state
                                .pending_devices
                                .iter()
                                .filter(|(_, q)| q.dedup_key == pending.dedup_key)
                                .map(|(k, _)| k.clone())
                                .collect();
                            for s in siblings {
                                if let Some(q) = state.pending_devices.remove(&s) {
                                    q.watcher.abort();
                                }
                            }
                        }
                    }
                    AddOutcome::ProbeFailed => {
                        park_device(&mut state, pending.dev, pending.attempts + 1, &reprobe_tx);
                    }
                    AddOutcome::NoDriver => {}
                }
                continue;
            }
            LoopEvent::Action(action) => action,
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

                let dev = HidrawDevice {
                    sysname: sysname.clone(),
                    devnode,
                    name,
                    bustype,
                    vid,
                    pid,
                    phys_path,
                    hid_uniq,
                };

                /* A single physical device (e.g. Logitech G403 HERO) can
                 * expose multiple hidraw nodes — one for the standard HID
                 * mouse interface and one for the vendor-specific HID++
                 * command channel.  Both nodes share the same USB topology
                 * path (phys_path) AND the same serial (hid_uniq); see
                 * HidrawDevice::dedup_key. */
                let dedup_key = dev.dedup_key();
                if !dev.phys_path.is_empty() && state.probed_devices.contains(&dedup_key) {
                    info!(
                        "Skipping {} ({:04x}:{:04x}): already probed on another hidraw node \
                         (phys={}, uniq={})",
                        sysname, vid, pid, dev.phys_path, dev.hid_uniq
                    );
                    continue;
                }

                /* A re-plug can reuse a sysname that is still parked from
                 * the previous plug cycle; drop the stale entry. */
                if let Some(stale) = state.pending_devices.remove(&sysname) {
                    stale.watcher.abort();
                }

                info!(
                    "Matched device: {} -> {} (driver: {})",
                    sysname, entry.name, entry.driver
                );

                match probe_and_register(&conn, entry, &dev, &mut state, MAX_PROBE_ATTEMPTS)
                    .await
                {
                    AddOutcome::Registered | AddOutcome::NoDriver => {}
                    AddOutcome::ProbeFailed => {
                        /* Do NOT register an empty D-Bus entry; park the
                         * device and re-probe when it shows signs of life. */
                        park_device(&mut state, dev, 0, &reprobe_tx);
                    }
                }
            }

            DeviceAction::Remove { sysname } => {
                /* Cancel any pending wake-watcher for the unplugged node. */
                if let Some(pending) = state.pending_devices.remove(&sysname) {
                    pending.watcher.abort();
                    info!("Removed parked device {}", sysname);
                }
                /* Clear the probed-device entry so a re-plugged device
                 * can be discovered again on a fresh hidraw node. */
                if let Some(key) = state.sysname_to_dedup_key.remove(&sysname) {
                    state.probed_devices.remove(&key);
                }
                if let Err(e) = remove_device(
                    &conn,
                    &sysname,
                    &mut state.registered_devices,
                    &mut state.actor_handles,
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

                state.registered_devices.insert(sysname, object_paths);
            }

            #[cfg(feature = "dev-hooks")]
            DeviceAction::RemoveTest { sysname } => {
                if let Err(e) = remove_device(
                    &conn,
                    &sysname,
                    &mut state.registered_devices,
                    &mut state.actor_handles,
                )
                .await
                {
                    warn!("Failed to remove test device {}: {e:#}", sysname);
                }
            }
        }
    }

    /* Cancel any wake-watchers still parked so the runtime does not
     * keep polling vanished nodes during shutdown. */
    for (_, pending) in state.pending_devices.drain() {
        pending.watcher.abort();
    }

    info!("udev monitor channel closed, shutting down");
    Ok(())
}
