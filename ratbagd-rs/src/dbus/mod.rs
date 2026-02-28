/* DBus surface: zbus interface implementations for Manager/Device/Profile/Resolution/Button/LED,
 * plus helpers to register devices and translate device actions from udev. */
pub mod button;
pub mod device;
pub mod led;
pub mod manager;
pub mod profile;
pub mod resolution;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};
use zbus::connection::Builder;
use zbus::zvariant::OwnedValue;

use crate::actor::{self, ActorHandle};
use crate::device::DeviceInfo;
use crate::device_database::{BusType, DeviceDb};
use crate::driver;
use crate::udev_monitor::DeviceAction;

/// Fallback [`OwnedValue`] (`u32` zero) used when zvariant serialization fails.
#[inline]
pub(crate) fn fallback_owned_value() -> OwnedValue {
    OwnedValue::from(0u32)
}

/// Register a new device and its children (profiles, buttons, etc) onto the DBus bus.
///
/// Returns a list of all object paths that were registered.
/// Child objects share the same `Arc<RwLock<DeviceInfo>>` so property
/// mutations propagate to the device-level `commit()` path.
async fn register_device_on_dbus(
    conn: &zbus::Connection,
    device_path: &str,
    shared_info: Arc<RwLock<DeviceInfo>>,
    actor_handle: Option<ActorHandle>,
) -> Vec<String> {
    let mut object_paths = Vec::with_capacity(64);
    object_paths.push(device_path.to_owned());
    let object_server = conn.object_server();

    // Register the Device object.
    let device_obj = device::RatbagDevice::new(
        Arc::clone(&shared_info),
        device_path.to_owned(),
        actor_handle,
    );

    if let Err(e) = object_server.at(device_path, device_obj).await {
        warn!("Failed to register device at {device_path}: {e}");
        return object_paths;
    }

    // Register Profile, Resolution, Button, LED child objects.
    // We snapshot the structure for iteration but children hold the shared
    // Arc so mutations propagate correctly to the commit path.
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
        object_paths.push(profile_path.clone());

        for res in &prof.resolutions {
            let res_path = format!("{device_path}/p{}/r{}", prof.index, res.index);
            let res_obj = resolution::RatbagResolution::new(
                Arc::clone(&shared_info),
                prof.index,
                res.index,
            );
            if let Err(e) = object_server.at(res_path.as_str(), res_obj).await {
                warn!("Failed to register resolution {res_path}: {e}");
            }
            object_paths.push(res_path);
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
            object_paths.push(btn_path);
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
            object_paths.push(led_path);
        }
    }
    
    object_paths
}

/// Unregister a device and all its children from the DBus object server,
/// then remove it from the manager's device list.
///
/// Shared between the `Remove` (udev) and `RemoveTest` (dev-hooks) paths.
async fn remove_device(
    conn: &zbus::Connection,
    sysname: &str,
    registered_devices: &mut HashMap<String, Vec<String>>,
    actor_handles: &mut HashMap<String, ActorHandle>,
) -> Result<()> {
    // Shut down the hardware actor if one is running.
    if let Some(handle) = actor_handles.remove(sysname) {
        handle.shutdown().await;
    }

    if let Some(paths) = registered_devices.remove(sysname) {
        let object_server = conn.object_server();

        // Remove child objects first (reverse order), then the device itself.
        // We attempt all interface types per path; only the matching one succeeds.
        for path in paths.iter().rev() {
            let _ = object_server
                .remove::<device::RatbagDevice, _>(path.as_str())
                .await;
            let _ = object_server
                .remove::<profile::RatbagProfile, _>(path.as_str())
                .await;
            let _ = object_server
                .remove::<resolution::RatbagResolution, _>(path.as_str())
                .await;
            let _ = object_server
                .remove::<button::RatbagButton, _>(path.as_str())
                .await;
            let _ = object_server
                .remove::<led::RatbagLed, _>(path.as_str())
                .await;
        }

        // The device root path is always paths[0]; update the manager list.
        let device_path = &paths[0];
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

    let conn = Builder::system()?
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

    // Track registered device paths so we can clean up on removal.
    let mut registered_devices: HashMap<String, Vec<String>> = HashMap::new();

    // Track actor handles so we can shut them down on removal.
    let mut actor_handles: HashMap<String, ActorHandle> = HashMap::new();

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
            } => {
                let key = (BusType::from_u16(bustype), vid, pid);

                let entry = match device_db.get(&key) {
                    Some(e) => e,
                    None => {
                        info!(
                            "Ignoring unsupported device {} ({:04x}:{:04x})",
                            sysname, vid, pid
                        );
                        continue;
                    }
                };

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
                let shared_info = Arc::new(RwLock::new(device_info));

                // Try to create and spawn the hardware driver actor.
                let actor_handle = match driver::create_driver(&entry.driver) {
                    Some(drv) => {
                        match actor::spawn_device_actor(
                            &devnode,
                            drv,
                            Arc::clone(&shared_info),
                        )
                        .await
                        {
                            Ok(handle) => {
                                info!(
                                    "Driver {} active for {}",
                                    entry.driver, sysname
                                );
                                Some(handle)
                            }
                            Err(e) => {
                                warn!(
                                    "Driver {} probe failed for {}: {e:#}",
                                    entry.driver, sysname
                                );
                                None
                            }
                        }
                    }
                    None => None,
                };

                let object_paths = register_device_on_dbus(
                    &conn,
                    &device_path,
                    Arc::clone(&shared_info),
                    actor_handle.clone(),
                )
                .await;

                // Update the manager's device list.
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

                if let Some(handle) = actor_handle {
                    actor_handles.insert(sysname.clone(), handle);
                }
                registered_devices.insert(sysname.clone(), object_paths);

                info!(
                    "Device {} registered at {} ({} child objects)",
                    entry.name,
                    device_path,
                    registered_devices[&sysname].len() - 1
                );
            }

            DeviceAction::Remove { sysname } => {
                remove_device(
                    &conn,
                    &sysname,
                    &mut registered_devices,
                    &mut actor_handles,
                )
                .await?;
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

                // Test devices have no hardware actor.
                let object_paths = register_device_on_dbus(
                    &conn,
                    &device_path,
                    Arc::clone(&shared_info),
                    None,
                )
                .await;

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

                registered_devices.insert(sysname, object_paths);
            }

            #[cfg(feature = "dev-hooks")]
            DeviceAction::RemoveTest { sysname } => {
                remove_device(
                    &conn,
                    &sysname,
                    &mut registered_devices,
                    &mut actor_handles,
                )
                .await?;
            }
        }
    }

    info!("udev monitor channel closed, shutting down");
    Ok(())
}
