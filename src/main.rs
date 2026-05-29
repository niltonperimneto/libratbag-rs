/* ratbagd-rs entrypoint: sets up tracing, loads the device database, spawns
 * the udev monitor, and starts the DBus server.  The main loop multiplexes
 * the DBus server, the udev monitor task, and a SIGINT handler so that a
 * failure in any subsystem or a Ctrl-C cleanly terminates the process. */

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tokio::signal;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

pub mod engine;
pub mod error;
pub mod hal;
pub mod ipc;
pub mod udev_monitor;

use crate::engine::device_database;

/* Channel capacity for udev hotplug events.  32 is generous for typical
 * hardware — even a full USB hub re-enumeration produces fewer events —
 * and keeps memory usage bounded while avoiding backpressure under normal
 * operating conditions. */
const DEVICE_CHANNEL_CAPACITY: usize = 32;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!(
        "Starting ratbagd-rs version {} (API version {}{})",
        env!("CARGO_PKG_VERSION"),
        ipc::manager::API_VERSION,
        if cfg!(feature = "dev-hooks") { ", dev-hooks enabled" } else { "" },
    );

    /* Load the .device file database from the project's data directory.
     * The directory must exist; an empty database means no device will ever
     * be recognised, which is almost certainly a packaging or path error. */
    let data_dir = PathBuf::from(
        std::env::var("RATBAGD_DATA_DIR")
            .unwrap_or_else(|_| "/usr/share/libratbag".to_string()),
    );

    if !data_dir.is_dir() {
        anyhow::bail!(
            "device database directory does not exist: {}",
            data_dir.display()
        );
    }

    let device_db = device_database::load_device_database(&data_dir);
    if device_db.is_empty() {
        warn!(
            "no .device files found in {} — no devices will be recognised",
            data_dir.display()
        );
    }

    let (device_tx, device_rx) = tokio::sync::mpsc::channel(DEVICE_CHANNEL_CAPACITY);

    /* Shared flag that tells the blocking udev thread to exit promptly
     * when the daemon receives a shutdown signal (Ctrl-C / SIGTERM).
     * Without this, the thread would sit in poll(2) for up to one second
     * before noticing that the mpsc channel has been closed. */
    let shutdown = Arc::new(AtomicBool::new(false));

    /* Spawn the udev monitor for hidraw device hotplug.  The handle is
     * joined inside the select! block so that a monitor failure or panic
     * is surfaced instead of silently lost. */
    let mut udev_handle = tokio::spawn(udev_monitor::run(device_tx, Arc::clone(&shutdown)));

    /* Multiplex the DBus server, udev monitor, and shutdown signal.
     * Whichever future completes first determines the exit path. */
    tokio::select! {
        result = ipc::run_server(device_rx, device_db) => {
            result?;
        }
        result = &mut udev_handle => {
            match result {
                Ok(Ok(())) => info!("udev monitor exited cleanly"),
                Ok(Err(e)) => anyhow::bail!("udev monitor failed: {e:#}"),
                Err(e)     => anyhow::bail!("udev monitor panicked: {e}"),
            }
        }
        _ = signal::ctrl_c() => {
            info!("received shutdown signal, exiting");
            /* Tell the blocking udev thread to exit on its next poll  */
            /* timeout instead of waiting for a channel-closed error.  */
            shutdown.store(true, Ordering::Relaxed);
            /* Abort the async wrapper so the runtime doesn't wait for */
            /* the spawned task after main() returns.                  */
            udev_handle.abort();
        }
    }

    Ok(())
}
