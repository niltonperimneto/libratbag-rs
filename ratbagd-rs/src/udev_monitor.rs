/* udev hotplug monitor: enumerates existing hidraw devices and dispatches add/remove (and dev-hook
 * test inject/remove) actions to the main DBus loop from a blocking thread. */
use std::os::unix::io::AsRawFd;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/* Actions dispatched from the udev monitor to the DBus server. */
#[derive(Debug)]
pub enum DeviceAction {
    Add {
        sysname: String,
        devnode: std::path::PathBuf,
        name: String,
        bustype: u16,
        vid: u16,
        pid: u16,
    },
    Remove {
        sysname: String,
    },
    /// Inject a synthetic test device directly into the DBus layer.
    ///
    /// Only constructed when the `dev-hooks` feature is enabled.
    #[cfg(feature = "dev-hooks")]
    InjectTest {
        sysname: String,
        device_info: crate::device::DeviceInfo,
    },
    /// Remove a previously-injected test device.
    ///
    /// Only constructed when the `dev-hooks` feature is enabled.
    #[cfg(feature = "dev-hooks")]
    RemoveTest {
        sysname: String,
    },
}

/* Run the udev monitor: enumerate existing hidraw devices, then watch */
/* for hotplug events indefinitely. */
/*  */
/* The `udev` crate types contain raw pointers and are not `Send`, */
/* so all udev operations run synchronously inside a blocking thread. */
pub async fn run(tx: mpsc::Sender<DeviceAction>) {
    info!("udev monitor started, watching for hidraw devices");

    let result = tokio::task::spawn_blocking(move || {
        run_blocking(tx)
    })
    .await;

    match result {
        Ok(Ok(())) => info!("udev monitor shutting down normally"),
        Ok(Err(e)) => warn!("udev monitor error: {}", e),
        Err(e) => warn!("udev monitor task panicked: {}", e),
    }
}

/* Synchronous udev monitor implementation that runs inside a blocking thread. */
fn run_blocking(tx: mpsc::Sender<DeviceAction>) -> Result<(), String> {
    /* Enumerate existing devices first */
    enumerate_existing(&tx)?;

    /* Set up the hotplug monitor */
    let monitor = udev::MonitorBuilder::new()
        .map_err(|e| format!("MonitorBuilder::new: {}", e))?
        .match_subsystem("hidraw")
        .map_err(|e| format!("match_subsystem: {}", e))?
        .listen()
        .map_err(|e| format!("listen: {}", e))?;

    info!("udev hotplug monitor listening on hidraw subsystem");

    /* Use poll(2) to wait for events on the udev monitor fd */
    let fd = monitor.as_raw_fd();

    loop {
        let mut pollfd = [nix::poll::PollFd::new(
            unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) },
            nix::poll::PollFlags::POLLIN,
        )];

        /* Block until the fd is readable (or timeout after 1 second to allow shutdown) */
        match nix::poll::poll(&mut pollfd, nix::poll::PollTimeout::from(1000u16)) {
            Ok(0) => continue, /* timeout, loop and re-check */
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(format!("poll: {}", e)),
        }

        /* Drain all pending events */
        for event in monitor.iter() {
            let event_type = event.event_type();

            match event_type {
                udev::EventType::Add => {
                    if let Some(action) = build_add_action(&event.device()) {
                        info!("Hotplug add: {}", action_sysname(&action));
                        /* Use blocking_send since we're in a sync context */
                        let _ = tx.blocking_send(action);
                    }
                }
                udev::EventType::Remove => {
                    let sysname = event
                        .device()
                        .sysname()
                        .to_string_lossy()
                        .to_string();
                    info!("Hotplug remove: {}", sysname);
                    let _ = tx.blocking_send(DeviceAction::Remove { sysname });
                }
                _ => {
                    /* Ignore bind/unbind/change events */
                }
            }
        }
    }
}

/* Enumerate all currently-connected hidraw devices and send `Add` actions. */
fn enumerate_existing(tx: &mpsc::Sender<DeviceAction>) -> Result<(), String> {
    let mut enumerator =
        udev::Enumerator::new().map_err(|e| format!("udev enumerator: {}", e))?;
    enumerator
        .match_subsystem("hidraw")
        .map_err(|e| format!("match_subsystem: {}", e))?;

    let devices = enumerator
        .scan_devices()
        .map_err(|e| format!("scan_devices: {}", e))?;

    for device in devices {
        if let Some(action) = build_add_action(&device) {
            debug!("Enumerated existing device: {}", action_sysname(&action));
            let _ = tx.blocking_send(action);
        }
    }

    Ok(())
}

/* Build a `DeviceAction::Add` from a udev device, extracting HID properties. */
fn build_add_action(device: &udev::Device) -> Option<DeviceAction> {
    let sysname = device.sysname().to_string_lossy().to_string();
    let devnode = device.devnode()?.to_path_buf();

    /* Walk up to the parent HID device to find HID_ID and HID_NAME */
    let hid_parent = find_hid_parent(device)?;

    let name = hid_parent
        .property_value("HID_NAME")
        .map(|v| v.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    let (bustype, vid, pid) = parse_hid_id(&hid_parent)?;

    Some(DeviceAction::Add {
        sysname,
        devnode,
        name,
        bustype,
        vid,
        pid,
    })
}

/* Walk up the device tree to find the parent with subsystem "hid". */
fn find_hid_parent(device: &udev::Device) -> Option<udev::Device> {
    let mut current = device.parent()?;
    loop {
        if let Some(subsystem) = current.subsystem()
            && subsystem == "hid"
        {
            return Some(current);
        }
        current = current.parent()?;
    }
}

/* Parse the `HID_ID` property (format: `BBBB:VVVV:PPPP`) into (bustype, vid, pid). */
fn parse_hid_id(device: &udev::Device) -> Option<(u16, u16, u16)> {
    let hid_id = device.property_value("HID_ID")?;
    let s = hid_id.to_string_lossy();
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }

    let bustype = u16::from_str_radix(parts[0], 16).ok()?;
    let vid = u16::from_str_radix(parts[1], 16).ok()?;
    let pid = u16::from_str_radix(parts[2], 16).ok()?;

    Some((bustype, vid, pid))
}

/* Helper to extract sysname from a DeviceAction for logging. */
fn action_sysname(action: &DeviceAction) -> &str {
    match action {
        DeviceAction::Add { sysname, .. } => sysname,
        DeviceAction::Remove { sysname } => sysname,
        #[cfg(feature = "dev-hooks")]
        DeviceAction::InjectTest { sysname, .. } => sysname,
        #[cfg(feature = "dev-hooks")]
        DeviceAction::RemoveTest { sysname } => sysname,
    }
}
