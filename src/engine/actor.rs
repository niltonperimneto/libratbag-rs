/* Device Actor — manages the lifecycle of a single connected device.
 *
 * Each physical device gets its own actor task (`tokio::spawn`), which
 * owns the `DeviceIo` file handle and the protocol driver instance.
 * DBus interface objects communicate with this actor through an
 * `mpsc` channel, ensuring that all hardware I/O is serialized. */

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{debug, info, warn};

use crate::engine::device::DeviceInfo;
use crate::hal::{DeviceDriver, DeviceIo};

/* Commands that DBus interface objects can send to the device actor. */
#[derive(Debug)]
pub enum ActorMessage {
    /* Commit all pending changes to hardware and report success/failure. */
    Commit {
        reply: oneshot::Sender<Result<(), String>>,
    },
    /* Gracefully shut down the actor (e.g., on device removal). */
    Shutdown,
}

/* Handle used by DBus objects to send commands to the device actor. */
#[derive(Clone)]
pub struct ActorHandle {
    tx: mpsc::Sender<ActorMessage>,
}

impl ActorHandle {
    /* Request the actor to shut down gracefully. */
    pub async fn shutdown(&self) {
        let _ = self.tx.send(ActorMessage::Shutdown).await;
    }

    /* Request the actor to commit pending changes to hardware.
     * Returns `Ok(())` on success, or an error string on failure. */
    pub async fn commit(&self) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.tx
            .send(ActorMessage::Commit { reply: reply_tx })
            .await
            .map_err(|_| "Device actor is no longer running".to_string())?;

        reply_rx
            .await
            .map_err(|_| "Device actor dropped the reply channel".to_string())?
    }
}

/* The device actor itself. Owns the I/O handle and driver instance. */
struct DeviceActor {
    driver: Box<dyn DeviceDriver>,
    io: DeviceIo,
    info: Arc<RwLock<DeviceInfo>>,
    rx: mpsc::Receiver<ActorMessage>,
}

impl DeviceActor {
    /* Main actor loop: process messages until shutdown or channel close. */
    async fn run(mut self) {
        info!(
            "Device actor started for {} (driver: {})",
            self.info.read().await.sysname,
            self.driver.name()
        );

        while let Some(msg) = self.rx.recv().await {
            match msg {
                ActorMessage::Commit { reply } => {
                    /* Clone a snapshot of the device state and release the
                     * lock immediately.  This prevents write-starvation:
                     * if the commit takes a long time (wireless retries,
                     * EEPROM writes), concurrent DBus writers are not
                     * blocked waiting for the read-lock to be released.
                     * The ~1.6 µs clone cost is negligible compared to the
                     * multi-millisecond hardware I/O that follows. */
                    let snapshot = self.info.read().await.clone();
                    let result = self.driver.commit(&mut self.io, &snapshot).await;

                    if result.is_ok() {
                        /* Clear dirty flags under a brief write-lock. */
                        let mut info = self.info.write().await;
                        *info = info.with_cleared_dirty_flags();
                    }

                    /* Process any unsolicited hardware events (e.g. profile
                     * switch notifications) that arrived during the commit's
                     * I/O calls.  These were buffered by DeviceIo::request()
                     * because they didn't match the pending command. */
                    let events = self.io.drain_events();
                    if !events.is_empty() {
                        let mut info = self.info.write().await;
                        for event in &events {
                            match self.driver.handle_event(event, &mut info).await {
                                Ok(true) => {
                                    debug!(
                                        "Unsolicited event updated device state: {:02x?}",
                                        event
                                    );
                                }
                                Ok(false) => { /* event was recognised but no state change */ }
                                Err(e) => {
                                    warn!("Error handling unsolicited event: {e}");
                                }
                            }
                        }
                    }

                    let response = result.map_err(|e| format!("{e:#}"));
                    let _ = reply.send(response);
                }
                ActorMessage::Shutdown => {
                    info!(
                        "Device actor shutting down for {}",
                        self.info.read().await.sysname
                    );
                    break;
                }
            }
        }

        debug!("Device actor loop exited");
    }
}

/* Maximum time allowed for the protocol probe phase (version ping +
 * feature discovery).  HID++ 2.0 probes up to two device indices;
 * a non-responding index burns one READ_TIMEOUT_PER_ATTEMPT (2 s)
 * cycle, so 8 seconds covers the worst case with headroom. */
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/* Maximum time allowed for loading profiles from hardware.  Complex
 * devices (e.g. G502 with 5 onboard profiles and multiple sector
 * reads) need several seconds; 15 seconds is generous even on a
 * congested wireless link. */
const LOAD_PROFILES_TIMEOUT: Duration = Duration::from_secs(15);

/* Spawn a device actor for the given hardware device.
 *
 * This function:
 * 1. Opens the `/dev/hidraw` device node.
 * 2. Probes the device with the protocol driver (with a timeout).
 * 3. Reads the full device state (profiles, DPIs, LEDs).
 * 4. Spawns the actor task and returns a handle for DBus objects.
 *
 * Returns `Err` if probing or profile loading fails or times out. */
pub async fn spawn_device_actor(
    devnode: &Path,
    mut driver: Box<dyn DeviceDriver>,
    info: Arc<RwLock<DeviceInfo>>,
) -> Result<ActorHandle> {
    let mut io = DeviceIo::open(devnode)
        .await
        .with_context(|| format!("Opening {}", devnode.display()))?;

    let driver_name = driver.name().to_string();
    let devnode_display = devnode.display().to_string();

    /* Probe and load_profiles have separate timeout budgets so that a
     * slow probe (e.g. a wired device that first tries the wrong
     * device index) does not eat into the time available for profile
     * loading, which involves many sector reads. */
    tokio::time::timeout(PROBE_TIMEOUT, async {
        driver
            .probe(&mut io)
            .await
            .with_context(|| format!("Probing {} with {}", devnode_display, driver_name))
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Probe timed out after {}s for {} with {}",
            PROBE_TIMEOUT.as_secs(),
            devnode.display(),
            driver.name()
        )
    })??;

    tokio::time::timeout(LOAD_PROFILES_TIMEOUT, async {
        let mut device_info = info.write().await;
        driver
            .load_profiles(&mut io, &mut device_info)
            .await
            .with_context(|| {
                format!(
                    "Loading profiles from {} with {}",
                    devnode_display, driver_name
                )
            })
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Profile loading timed out after {}s for {} with {}",
            LOAD_PROFILES_TIMEOUT.as_secs(),
            devnode.display(),
            driver.name()
        )
    })??;

    /* Create the message channel and spawn the actor */
    let (tx, rx) = mpsc::channel(16);

    let actor = DeviceActor {
        driver,
        io,
        info,
        rx,
    };

    tokio::spawn(async move {
        actor.run().await;
    });

    Ok(ActorHandle { tx })
}
