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

/* Upper bound on reports drained per idle wakeup, so a flood of input
 * reports cannot starve pending actor messages. */
const MAX_IDLE_DRAIN: usize = 32;

/* What woke the actor loop up. */
enum Wakeup {
    Message(Option<ActorMessage>),
    IdleReadable(Result<(), crate::hal::DriverError>),
}

/* The device actor itself. Owns the I/O handle and driver instance. */
struct DeviceActor {
    driver: Box<dyn DeviceDriver>,
    io: DeviceIo,
    info: Arc<RwLock<DeviceInfo>>,
    rx: mpsc::Receiver<ActorMessage>,
    /* Fired (best-effort) whenever an unsolicited hardware event changed
     * the shared device state, so a future consumer can emit D-Bus
     * signals.  `None` disables notification. */
    notify_tx: Option<mpsc::UnboundedSender<()>>,
}

impl DeviceActor {
    /* Main actor loop: process messages until shutdown or channel close.
     *
     * When the driver opts in via `wants_unsolicited_events()`, the loop
     * also watches the device fd between commands and feeds unsolicited
     * reports (profile/DPI switches from physical buttons) to
     * `DeviceDriver::handle_event` — previously these were only seen
     * after a commit happened to run.  `biased` keeps command handling
     * ahead of event draining, and the readiness future is only polled
     * between commands, so it can never interleave with an in-flight
     * `request()` during a commit. */
    async fn run(mut self) {
        info!(
            "Device actor started for {} (driver: {})",
            self.info.read().await.sysname,
            self.driver.name()
        );

        let mut watch_idle = self.driver.wants_unsolicited_events();

        loop {
            /* Both arms are cancel-safe: `mpsc::recv` and the readiness
             * wait consume nothing when their future is dropped. */
            let wakeup = tokio::select! {
                biased;
                msg = self.rx.recv() => Wakeup::Message(msg),
                result = self.io.wait_readable(), if watch_idle => {
                    Wakeup::IdleReadable(result)
                }
            };

            match wakeup {
                Wakeup::Message(Some(ActorMessage::Commit { reply })) => {
                    self.handle_commit(reply).await;
                }
                Wakeup::Message(Some(ActorMessage::Shutdown)) => {
                    info!(
                        "Device actor shutting down for {}",
                        self.info.read().await.sysname
                    );
                    break;
                }
                Wakeup::Message(None) => break,
                Wakeup::IdleReadable(Ok(())) => {
                    self.process_unsolicited().await;
                }
                Wakeup::IdleReadable(Err(e)) => {
                    /* ENODEV after unplug and similar: stop watching
                     * instead of spinning; the udev Remove → Shutdown
                     * message ends the loop shortly. */
                    warn!(
                        "Idle watch failed on {}: {e}; disabling idle event listener",
                        self.io.path().display()
                    );
                    watch_idle = false;
                }
            }
        }

        debug!("Device actor loop exited");
    }

    /* Commit pending changes to hardware and reply to the requester. */
    async fn handle_commit(&mut self, reply: oneshot::Sender<Result<(), String>>) {
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
        self.handle_unsolicited_reports(events).await;

        let response = result.map_err(|e| format!("{e:#}"));
        let _ = reply.send(response);
    }

    /* Drain reports queued on the idle fd and feed the HID++ ones to
     * the driver.  Never blocks: uses the non-blocking read so command
     * messages regain control as soon as the queue is empty. */
    async fn process_unsolicited(&mut self) {
        let mut buf = [0u8; 64];
        let mut reports: Vec<Vec<u8>> = Vec::new();

        for _ in 0..MAX_IDLE_DRAIN {
            match self.io.try_read_report(&mut buf) {
                Ok(Some(n))
                    if n > 0
                        && (buf[0] == crate::hal::HIDPP_SHORT_REPORT_ID
                            || buf[0] == crate::hal::HIDPP_LONG_REPORT_ID) =>
                {
                    reports.push(buf[..n].to_vec());
                }
                /* Non-HID++ noise (motion/keyboard input): discard. */
                Ok(Some(_)) => continue,
                /* Queue drained. */
                Ok(None) => break,
                Err(e) => {
                    warn!("Idle read failed on {}: {e:#}", self.io.path().display());
                    break;
                }
            }
        }

        /* Include anything buffered by earlier request() calls. */
        reports.extend(self.io.drain_events());
        self.handle_unsolicited_reports(reports).await;
    }

    /* Feed unsolicited reports to the driver under a single write-lock
     * and fire the change notification if any of them altered state. */
    async fn handle_unsolicited_reports(&mut self, reports: Vec<Vec<u8>>) {
        if reports.is_empty() {
            return;
        }

        let mut changed = false;
        {
            let mut info = self.info.write().await;
            for report in &reports {
                match self.driver.handle_event(report, &mut info).await {
                    Ok(true) => {
                        changed = true;
                        debug!("Unsolicited event updated device state: {:02x?}", report);
                    }
                    Ok(false) => { /* recognised but no state change */ }
                    Err(e) => {
                        warn!("Error handling unsolicited event: {e}");
                    }
                }
            }
        }

        if changed
            && let Some(tx) = &self.notify_tx
        {
            let _ = tx.send(());
        }
    }
}

/* Maximum time allowed for the protocol probe phase (version ping +
 * feature discovery).  The HID++ drivers probe up to 2 device indices
 * with up to 2 attempts each, and every silent attempt burns one full
 * READ_TIMEOUT_PER_ATTEMPT (2 s, see hal/mod.rs) — 8 s worst case, so
 * 10 seconds leaves headroom for feature discovery.  Keep this in sync
 * with PROBE_INDICES/PROBE_ATTEMPTS in hidpp10.rs and hidpp20.rs. */
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

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
 * Returns `Err` if probing or profile loading fails or times out.
 *
 * `notify_tx`, when provided, is fired every time an unsolicited
 * hardware event changes the shared device state (e.g. the user
 * switches profiles with a physical button), so the D-Bus layer can
 * emit change signals.  Pass `None` when no signalling is needed —
 * the shared `DeviceInfo` is updated either way. */
pub async fn spawn_device_actor(
    devnode: &Path,
    mut driver: Box<dyn DeviceDriver>,
    info: Arc<RwLock<DeviceInfo>>,
    notify_tx: Option<mpsc::UnboundedSender<()>>,
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
        notify_tx,
    };

    tokio::spawn(async move {
        actor.run().await;
    });

    Ok(ActorHandle { tx })
}
