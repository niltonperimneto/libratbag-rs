/* ratbagd-rs entrypoint: sets up tracing, loads the device database, spawns the udev monitor,
 * and starts the DBus server. */
mod actor;
mod dbus;
mod device;
mod device_database;
mod driver;
mod error;
#[cfg(feature = "dev-hooks")]
mod test_device;
mod udev_monitor;

use std::path::PathBuf;

use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!(
        "Starting ratbagd-rs version {} (API version {})",
        env!("CARGO_PKG_VERSION"),
        dbus::manager::API_VERSION
    );

    /* Load the .device file database from the project's data directory */
    let data_dir = PathBuf::from(
        std::env::var("RATBAGD_DATA_DIR")
            .unwrap_or_else(|_| "/usr/share/libratbag".to_string()),
    );
    let device_db = device_database::load_device_database(&data_dir);

    let (device_tx, device_rx) = tokio::sync::mpsc::channel(32);

    /* Spawn the udev monitor for hidraw device hotplug */
    tokio::spawn(udev_monitor::run(device_tx));

    /* Run the DBus server (blocks until shutdown) */
    dbus::run_server(device_rx, device_db).await?;

    Ok(())
}
