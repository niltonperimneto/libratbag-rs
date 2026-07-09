/* Driver framework: DeviceDriver trait, DeviceIo HID helpers, driver factory, and shared driver
 * error types used by all protocol implementations. */
pub mod asus;
pub mod etekcity;
pub mod gskill;
pub mod hidpp;
pub mod hidpp10;
pub mod hidpp20;
pub mod logitech_g300;
pub mod logitech_g600;
pub mod marsgaming;
pub mod openinput;
pub mod roccat;
pub mod sinowealth;
pub mod sinowealth_nubwo;
pub mod steelseries;

use nix::libc;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use thiserror::Error;
use tokio::io::unix::AsyncFd;
use tracing::{debug, trace, warn};

use crate::engine::device::DeviceInfo;

/* Domain-specific error variants for all driver I/O operations. */
/*                                                                 */
/* Using explicit variants instead of opaque strings allows the    */
/* daemon to take structured recovery actions (e.g., retrying on   */
/* `Timeout` vs. logging and abandoning on `ChecksumMismatch`).   */
#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum DriverError {
    #[error("I/O failure on {device}: {source}")]
    Io {
        device: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Feature report ioctl failed: {0}")]
    IoctlFailed(std::io::Error),

    #[error("Hardware timed out after {attempts} attempt(s)")]
    Timeout { attempts: u8 },

    #[error("Checksum mismatch: computed {computed:#06x}, received {received:#06x}")]
    ChecksumMismatch { computed: u16, received: u16 },

    #[error("Device reported protocol error (sub_id={sub_id:#04x}, error={error:#04x})")]
    ProtocolError { sub_id: u8, error: u8 },

    #[error("Invalid buffer size: expected at least {expected}, got {actual}")]
    BufferTooSmall { expected: usize, actual: usize },

    #[error(
        "HID++ 2.0 error {error_name} (0x{error_code:02X}) \
         for feature 0x{feature_index:02X} fn={function}"
    )]
    Hidpp20Error {
        error_name: &'static str,
        error_code: u8,
        feature_index: u8,
        function: u8,
    },

    #[error("HID++ 2.0 probe failed: no device responded (tried indices: {indices:02X?})")]
    Hidpp20ProbeFailure { indices: Vec<u8> },
}

/* Maximum HID report size.                                        */
/*                                                                 */
/* Roccat macros are the largest at 2082 bytes. We use 4096 as    */
/* a safe ceiling covering any current and future HID report.     */
#[allow(dead_code)]
const MAX_REPORT_LEN: usize = 4096;

/* Total time budget for each attempt's read loop.                */
/*                                                                */
/* Wireless HID++ devices multiplex protocol responses with       */
/* normal mouse input reports on the same hidraw node. The mouse  */
/* may emit dozens of input reports per millisecond, so a purely  */
/* count-based loop is insufficient — the budget is exhausted     */
/* before the protocol response arrives. This time-based approach */
/* keeps reading and discarding non-matching reports until the    */
/* deadline expires or a match is found.                          */
/*                                                                */
/* NOTE: probe budgets depend on this value — see PROBE_TIMEOUT   */
/* in engine/actor.rs, which must stay >= (probe indices) ×       */
/* (probe attempts) × READ_TIMEOUT_PER_ATTEMPT.                   */
const READ_TIMEOUT_PER_ATTEMPT: Duration = Duration::from_millis(2000);

/* Upper bound on buffered unsolicited events.  A chatty device    */
/* during a long commit could otherwise grow the buffer without   */
/* limit; beyond the cap the oldest event is dropped.             */
const MAX_PENDING_EVENTS: usize = 64;

/* HID++ report ID prefixes. Any report whose first byte is NOT   */
/* one of these is a regular HID input report (mouse movement,    */
/* keyboard, etc.) and should be silently skipped.                */
const HIDPP_SHORT_REPORT_ID: u8 = 0x10;
const HIDPP_LONG_REPORT_ID: u8 = 0x11;

/* Compute the `HIDIOCGFEATURE(len)` ioctl request number.        */
/*                                                                */
/* Linux hidraw.h: `_IOC(_IOC_READ|_IOC_WRITE, 'H', 0x07, len)`. */
fn hid_get_feature_req(len: usize) -> libc::c_ulong {
    let ioc_readwrite: libc::c_ulong = 3;
    let ioc_type: libc::c_ulong = b'H' as libc::c_ulong;
    let ioc_nr: libc::c_ulong = 0x07;
    (ioc_readwrite << 30) | (ioc_type << 8) | ioc_nr | ((len as libc::c_ulong) << 16)
}

/* Compute the `HIDIOCSFEATURE(len)` ioctl request number.        */
/*                                                                */
/* Linux hidraw.h: `_IOC(_IOC_READ|_IOC_WRITE, 'H', 0x06, len)`. */
#[allow(dead_code)]
fn hid_set_feature_req(len: usize) -> libc::c_ulong {
    let ioc_readwrite: libc::c_ulong = 3;
    let ioc_type: libc::c_ulong = b'H' as libc::c_ulong;
    let ioc_nr: libc::c_ulong = 0x06;
    (ioc_readwrite << 30) | (ioc_type << 8) | ioc_nr | ((len as libc::c_ulong) << 16)
}

/* Transport behind `DeviceIo`: the real hidraw file in production, */
/* or a scripted in-memory device in unit tests.                     */
enum IoBackend {
    File(AsyncFd<std::fs::File>),
    #[cfg(test)]
    Mock(mock::MockHid),
}

/* Async wrapper around a `/dev/hidraw` file descriptor.           */
/*                                                                 */
/* All hardware I/O goes through this struct so that drivers never */
/* touch raw file handles directly.                                */
/*                                                                 */
/* The fd is opened with O_NONBLOCK and driven through `AsyncFd`   */
/* readiness (epoll), NOT through `tokio::fs::File`.  The blocking */
/* threadpool `File` is unusable here: a `tokio::time::timeout`    */
/* around one of its reads cancels only the future, leaving the    */
/* blocking read(2) in flight — the handle stays busy so the next  */
/* write stalls until the device emits *any* report, and the data  */
/* from the late-completing read is silently discarded.  With      */
/* AsyncFd, dropping a cancelled read future consumes nothing from */
/* the fd, so timeouts and retries behave as written.              */
pub struct DeviceIo {
    backend: IoBackend,
    path: std::path::PathBuf,
    /* Reports seen during `request()` that were valid HID++ but did not
     * match the pending command.  These are unsolicited hardware events
     * (e.g. profile-switch notifications) that the actor should forward
     * to `DeviceDriver::handle_event` after each I/O batch. */
    pending_events: Vec<Vec<u8>>,
}

impl DeviceIo {
    /* Open the hidraw device node at `path`.                          */
    /*                                                                 */
    /* Kept `async` for API stability even though hidraw open(2) never */
    /* blocks meaningfully; every caller already awaits this.          */
    pub async fn open(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .with_context(|| format!("Failed to open hidraw device {}", path.display()))?;

        Self::from_std(file, path.to_path_buf())
    }

    /* Wrap an already-open non-blocking file.  Split out from `open`  */
    /* so tests can drive DeviceIo over a socketpair fake device.      */
    pub(crate) fn from_std(file: std::fs::File, path: std::path::PathBuf) -> Result<Self> {
        let fd = AsyncFd::new(file).with_context(|| {
            format!("Failed to register {} with the async reactor", path.display())
        })?;

        Ok(Self {
            backend: IoBackend::File(fd),
            path,
            pending_events: Vec::new(),
        })
    }

    /* Build a `DeviceIo` backed by a scripted mock device, along with a
     * handle for inspecting the traffic after the fact.  Lets driver
     * probe/load/commit flows run in unit tests without hardware. */
    #[cfg(test)]
    pub(crate) fn with_mock(script: Vec<mock::MockExchange>) -> (Self, mock::MockHandle) {
        let (hid, handle) = mock::MockHid::scripted(script);
        (
            Self {
                backend: IoBackend::Mock(hid),
                path: std::path::PathBuf::from("/dev/mock-hidraw"),
                pending_events: Vec::new(),
            },
            handle,
        )
    }

    /* Return the path of the underlying hidraw device node. */
    pub fn path(&self) -> &Path {
        &self.path
    }

    /* Write a raw HID report to the device.                          */
    /*                                                                */
    /* hidraw writes complete synchronously, so the writable loop is  */
    /* effectively a single iteration; the loop only exists to retry  */
    /* a spurious-wakeup EAGAIN.                                      */
    pub async fn write_report(&mut self, buf: &[u8]) -> Result<(), DriverError> {
        match &mut self.backend {
            IoBackend::File(fd) => {
                loop {
                    let mut guard = fd
                        .writable()
                        .await
                        .map_err(|source| DriverError::Io {
                            device: self.path.display().to_string(),
                            source,
                        })?;

                    match guard.try_io(|inner| (&mut &*inner.get_ref()).write(buf)) {
                        Ok(Ok(n)) if n == buf.len() => {
                            debug!("TX {} bytes: {:02x?}", n, buf);
                            return Ok(());
                        }
                        Ok(Ok(n)) => {
                            return Err(DriverError::Io {
                                device: self.path.display().to_string(),
                                source: std::io::Error::new(
                                    std::io::ErrorKind::WriteZero,
                                    format!("Short write: {}/{} bytes", n, buf.len()),
                                ),
                            });
                        }
                        Ok(Err(source)) => {
                            return Err(DriverError::Io {
                                device: self.path.display().to_string(),
                                source,
                            });
                        }
                        Err(_would_block) => continue,
                    }
                }
            }
            #[cfg(test)]
            IoBackend::Mock(hid) => {
                hid.write_report(buf).map_err(|source| DriverError::Io {
                    device: self.path.display().to_string(),
                    source: std::io::Error::new(std::io::ErrorKind::Other, source),
                })
            }
        }
    }

    /* Read a single HID report from the device (waits until one is    */
    /* queued).  Cancel-safe: dropping the returned future between     */
    /* polls consumes nothing from the fd, so a `tokio::time::timeout` */
    /* wrapper genuinely abandons the read.                            */
    pub async fn read_report(&mut self, buf: &mut [u8]) -> Result<usize, DriverError> {
        match &mut self.backend {
            IoBackend::File(fd) => {
                loop {
                    let mut guard = fd
                        .readable()
                        .await
                        .map_err(|source| DriverError::Io {
                            device: self.path.display().to_string(),
                            source,
                        })?;

                    match guard.try_io(|inner| (&mut &*inner.get_ref()).read(buf)) {
                        Ok(Ok(n)) => {
                            debug!("RX {} bytes: {:02x?}", n, &buf[..n]);
                            return Ok(n);
                        }
                        Ok(Err(source)) => {
                            return Err(DriverError::Io {
                                device: self.path.display().to_string(),
                                source,
                            });
                        }
                        Err(_would_block) => continue,
                    }
                }
            }
            #[cfg(test)]
            IoBackend::Mock(hid) => {
                hid.read_report(buf).await.map_err(|source| DriverError::Io {
                    device: self.path.display().to_string(),
                    source: std::io::Error::new(std::io::ErrorKind::Other, source),
                })
            }
        }
    }

    /* Wait until the device has at least one report queued, WITHOUT  */
    /* consuming it.  The readiness guard is dropped without          */
    /* `clear_ready()`, so a subsequent read sees the data.           */
    /*                                                                */
    /* Used by the actor's idle event listener and by the wake-       */
    /* watcher for parked (probe-failed) devices.  Cancel-safe.       */
    pub async fn wait_readable(&self) -> Result<(), DriverError> {
        match &self.backend {
            IoBackend::File(fd) => {
                fd.readable()
                    .await
                    .map_err(|source| DriverError::Io {
                        device: self.path.display().to_string(),
                        source,
                    })?;
                Ok(())
            }
            #[cfg(test)]
            IoBackend::Mock(hid) => {
                let queued = {
                    let state = hid.state.lock().unwrap();
                    !state.queued.is_empty()
                };
                if queued {
                    Ok(())
                } else {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        }
    }

    /* Non-blocking single read: `Ok(Some(n))` if a report was queued, */
    /* `Ok(None)` if the fd has no data (EAGAIN).  Used by the actor's */
    /* idle drain so it never blocks between commands.                 */
    pub fn try_read_report(&mut self, buf: &mut [u8]) -> Result<Option<usize>, DriverError> {
        match &mut self.backend {
            IoBackend::File(fd) => {
                match (&mut &*fd.get_ref()).read(buf) {
                    Ok(n) => {
                        debug!("RX {} bytes: {:02x?}", n, &buf[..n]);
                        Ok(Some(n))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                    Err(source) => {
                        Err(DriverError::Io {
                            device: self.path.display().to_string(),
                            source,
                        })
                    }
                }
            }
            #[cfg(test)]
            IoBackend::Mock(hid) => {
                let mut state = hid.state.lock().unwrap();
                match state.queued.pop_front() {
                    Some(data) => {
                        let n = data.len().min(buf.len());
                        buf[..n].copy_from_slice(&data[..n]);
                        Ok(Some(n))
                    }
                    None => Ok(None),
                }
            }
        }
    }

    /* Get a HID feature report using the `HIDIOCGFEATURE` ioctl.  */
    /*                                                             */
    /* `buf[0]` must contain the report ID before calling; the     */
    /* kernel fills the remaining bytes with the report data and   */
    /* returns the total number of bytes written.                  */
    pub fn get_feature_report(&self, buf: &mut [u8]) -> Result<usize, DriverError> {
        let fd = self.raw_fd()?;
        let req = hid_get_feature_req(buf.len());

        /* SAFETY: `fd` is a valid open file descriptor for the     */
        /* lifetime of this call. `buf` is a live mutable slice and */
        /* its length is encoded into `req` via the ioctl macro.    */
        /* The kernel reads exactly `buf.len()` bytes from this fd. */
        let res = unsafe { libc::ioctl(fd, req, buf.as_mut_ptr()) };

        if res < 0 {
            return Err(DriverError::IoctlFailed(std::io::Error::last_os_error()));
        }

        let n = res as usize;
        debug!("GET_FEATURE {} bytes: {:02x?}", n, &buf[..n]);
        Ok(n)
    }

    /* Set a HID feature report using the `HIDIOCSFEATURE` ioctl.  */
    /*                                                             */
    /* `buf[0]` must contain the report ID. Returns the number of  */
    /* bytes accepted by the kernel.                               */
    pub fn set_feature_report(&self, buf: &[u8]) -> Result<usize, DriverError> {
        let fd = self.raw_fd()?;
        let req = hid_set_feature_req(buf.len());

        /* SAFETY: `fd` is a valid open file descriptor for the     */
        /* lifetime of this call. `buf` is a live immutable slice   */
        /* and its length is encoded into `req` via the ioctl macro. */
        /* The kernel reads exactly `buf.len()` bytes from this fd. */
        let res = unsafe { libc::ioctl(fd, req, buf.as_ptr()) };

        if res < 0 {
            return Err(DriverError::IoctlFailed(std::io::Error::last_os_error()));
        }

        let n = res as usize;
        debug!("SET_FEATURE {} bytes: {:02x?}", n, &buf[..n]);
        Ok(n)
    }

    /* Return the raw fd for feature-report ioctls.  Only the file  */
    /* backend has one; the mock backend does not support feature   */
    /* reports (no driver under test currently needs them).         */
    fn raw_fd(&self) -> Result<std::os::unix::io::RawFd, DriverError> {
        match &self.backend {
            IoBackend::File(file) => Ok(file.as_raw_fd()),
            #[cfg(test)]
            IoBackend::Mock(_) => Err(DriverError::IoctlFailed(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "feature reports are not supported by the mock backend",
            ))),
        }
    }

    /* Send a report and wait for a matching response.             */
    /*                                                             */
    /* The `matcher` closure receives each incoming HID++ report   */
    /* and returns `Some(T)` when the expected response has        */
    /* arrived, or `None` to keep waiting.                         */
    /*                                                             */
    /* The read loop is TIME-based, not count-based, because       */
    /* wireless receivers multiplex HID++ protocol responses with  */
    /* regular mouse input reports on the same hidraw node. The    */
    /* mouse can emit hundreds of input reports per second, so a   */
    /* count-based loop would be exhausted before the protocol     */
    /* response arrives. Non-HID++ reports (those not starting     */
    /* with 0x10 or 0x11) are silently discarded.                  */
    pub async fn request<T, F>(
        &mut self,
        report: &[u8],
        report_size: usize,
        max_attempts: u8,
        mut matcher: F,
    ) -> Result<T, DriverError>
    where
        F: FnMut(&[u8]) -> Option<T>,
    {
        /* Stack-allocated read buffer — avoids a heap allocation per    */
        /* attempt.  64 bytes covers all HID++ report sizes (short = 7, */
        /* long = 20, very-long = 64).  We slice to `report_size` for   */
        /* the actual read so callers see exactly the length they asked  */
        /* for.                                                          */
        const MAX_HID_REPORT: usize = 64;
        if report_size > MAX_HID_REPORT {
            return Err(DriverError::BufferTooSmall {
                expected: MAX_HID_REPORT,
                actual: report_size,
            });
        }

        for attempt in 1..=max_attempts {
            self.write_report(report).await?;

            /* Each attempt gets the FULL read budget.  A silent window
             * no longer aborts the attempt early: reads are genuinely
             * cancellable now, so waiting out the whole deadline costs
             * nothing and gives slow wireless links (e.g. a mouse that
             * just woke from sleep) time to answer. */
            let deadline = tokio::time::Instant::now() + READ_TIMEOUT_PER_ATTEMPT;
            let mut backing = [0u8; MAX_HID_REPORT];
            let buf = &mut backing[..report_size];

            loop {
                match tokio::time::timeout_at(deadline, self.read_report(buf)).await {
                    Ok(Ok(n)) => {
                        /* Skip non-HID++ input reports (mouse movement, */
                        /* keyboard, etc.) — they are noise here.        */
                        if n > 0
                            && buf[0] != HIDPP_SHORT_REPORT_ID
                            && buf[0] != HIDPP_LONG_REPORT_ID
                        {
                            continue;
                        }

                        if let Some(result) = matcher(&buf[..n]) {
                            return Ok(result);
                        }

                        /* The report was valid HID++ but did not match our
                         * pending command — buffer it as an unsolicited
                         * hardware event for the actor to process later. */
                        self.push_pending_event(buf[..n].to_vec());
                    }
                    Ok(Err(e)) => {
                        warn!("Read error on attempt {attempt}: {e}");
                        break;
                    }
                    Err(_elapsed) => {
                        /* Attempt deadline expired: retry with a fresh */
                        /* write (or give up after the last attempt).   */
                        trace!("Read deadline expired on attempt {attempt}");
                        break;
                    }
                }
            }
        }

        Err(DriverError::Timeout {
            attempts: max_attempts,
        })
    }

    /* Buffer an unsolicited HID++ report, dropping the oldest entry
     * once the cap is reached so a chatty device cannot grow the
     * buffer without bound during a long commit. */
    fn push_pending_event(&mut self, event: Vec<u8>) {
        if self.pending_events.len() >= MAX_PENDING_EVENTS {
            warn!(
                "Pending event buffer full ({MAX_PENDING_EVENTS}); dropping oldest event"
            );
            self.pending_events.remove(0);
        }
        self.pending_events.push(event);
    }

    /* Drain all unsolicited HID++ events that were buffered during
     * `request()` calls.  The actor calls this after each I/O batch
     * and forwards the reports to `DeviceDriver::handle_event`. */
    pub fn drain_events(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_events)
    }
}

/* Scripted in-memory HID device for driver unit tests.             */
/*                                                                  */
/* A script is a sequence of exchanges consumed one per write: each */
/* write pops the next exchange (optionally asserting the written   */
/* bytes) and queues its reply for the following read.  A read with */
/* nothing queued pends forever, exactly like a mute hidraw node —  */
/* combine with `tokio::test(start_paused = true)` to exercise      */
/* driver timeout paths instantly.                                  */
#[cfg(test)]
pub(crate) mod mock {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use anyhow::{bail, Result};

    /* What the mock device does after accepting a write. */
    pub enum MockReply {
        /* Deliver these bytes on the next read. */
        Data(Vec<u8>),
        /* Stay silent: the next read pends forever (timeout testing). */
        Silence,
        /* Fail the write itself with a hard I/O error. */
        WriteError,
    }

    /* One scripted write→reply exchange. */
    pub struct MockExchange {
        /* When `Some`, the written report must match these bytes exactly. */
        pub expect: Option<Vec<u8>>,
        pub reply: MockReply,
    }

    impl MockExchange {
        pub fn reply(data: Vec<u8>) -> Self {
            Self { expect: None, reply: MockReply::Data(data) }
        }

        pub fn expect_reply(expect: Vec<u8>, data: Vec<u8>) -> Self {
            Self { expect: Some(expect), reply: MockReply::Data(data) }
        }
    }

    #[derive(Default)]
    struct MockState {
        script: VecDeque<MockExchange>,
        writes: Vec<Vec<u8>>,
        queued: VecDeque<Vec<u8>>,
    }

    pub(crate) struct MockHid {
        state: Arc<Mutex<MockState>>,
    }

    /* Test-side view of the mock: inspect traffic after driving the driver. */
    #[derive(Clone)]
    pub(crate) struct MockHandle {
        state: Arc<Mutex<MockState>>,
    }

    impl MockHid {
        pub(crate) fn scripted(script: Vec<MockExchange>) -> (Self, MockHandle) {
            let state = Arc::new(Mutex::new(MockState {
                script: script.into(),
                ..MockState::default()
            }));
            (Self { state: state.clone() }, MockHandle { state })
        }

        pub(super) fn write_report(&mut self, buf: &[u8]) -> Result<()> {
            let mut st = self.state.lock().unwrap();
            st.writes.push(buf.to_vec());

            let Some(exchange) = st.script.pop_front() else {
                bail!("MockHid: unexpected write, script exhausted: {:02x?}", buf);
            };
            if let Some(expect) = &exchange.expect
                && buf != &expect[..]
            {
                bail!(
                    "MockHid: write mismatch\n  expected: {:02x?}\n  actual:   {:02x?}",
                    expect, buf
                );
            }
            match exchange.reply {
                MockReply::Data(data) => st.queued.push_back(data),
                MockReply::Silence => { /* nothing queued: next read pends */ }
                MockReply::WriteError => bail!("MockHid: scripted write error"),
            }
            Ok(())
        }

        pub(super) async fn read_report(&mut self, buf: &mut [u8]) -> Result<usize> {
            let reply = self.state.lock().unwrap().queued.pop_front();
            match reply {
                Some(data) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Ok(n)
                }
                /* Mute device: never resolves, mirrors a real hidraw
                 * node with no data. Callers race this against a timeout. */
                None => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        }
    }

    impl MockHandle {
        /* All reports written to the device, in order. */
        pub(crate) fn writes(&self) -> Vec<Vec<u8>> {
            self.state.lock().unwrap().writes.clone()
        }

        /* True once every scripted exchange has been consumed. */
        pub(crate) fn script_exhausted(&self) -> bool {
            self.state.lock().unwrap().script.is_empty()
        }
    }
}

/* The universal driver interface for all hardware protocols.      */
/*                                                                 */
/* Every supported protocol (HID++ 1.0, HID++ 2.0, Roccat, etc.) */
/* implements this trait. The daemon calls these methods from the  */
/* device actor loop.                                              */
#[async_trait]
pub trait DeviceDriver: Send + Sync {
    /* Returns the driver name for logging purposes. */
    fn name(&self) -> &str;

    /* Probe the device to confirm it speaks this protocol.        */
    /*                                                             */
    /* For HID++ this sends a version ping; for other protocols it */
    /* will send an equivalent handshake. Returns `Ok(())` if the */
    /* device responded correctly.                                 */
    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()>;

    /* Read the full device state (profiles, DPIs, buttons, LEDs) */
    /* from hardware into the `DeviceInfo` struct.                 */
    async fn load_profiles(&mut self, io: &mut DeviceIo, info: &mut DeviceInfo) -> Result<()>;

    /* Write the modified device state back to hardware.           */
    /*                                                             */
    /* Only dirty fields should be transmitted; the driver should  */
    /* diff the `DeviceInfo` against its internal cached state.    */
    async fn commit(&mut self, io: &mut DeviceIo, info: &DeviceInfo) -> Result<()>;

    /* Handle an unsolicited hardware event (e.g. profile switch,  */
    /* DPI change triggered by a physical button on the device).   */
    /*                                                             */
    /* Returns `true` if the event caused a state change in `info` */
    /* that the actor should propagate via DBus signals.           */
    /*                                                             */
    /* The default implementation ignores all events.              */
    async fn handle_event(
        &mut self,
        _report: &[u8],
        _info: &mut DeviceInfo,
    ) -> Result<bool> {
        Ok(false)
    }
}

/* Instantiate the correct driver based on the driver name from the */
/* `.device` file database.                                         */
pub fn create_driver(driver_name: &str) -> Option<Box<dyn DeviceDriver>> {
    match driver_name {
        "asus" => Some(Box::new(asus::AsusDriver::new())),
        "etekcity" => Some(Box::new(etekcity::EtekcityDriver::new())),
        "gskill" => Some(Box::new(gskill::GskillDriver::new())),
        "hidpp10" => Some(Box::new(hidpp10::Hidpp10Driver::new())),
        "hidpp20" => Some(Box::new(hidpp20::Hidpp20Driver::new())),
        "logitech_g300" => Some(Box::new(logitech_g300::LogitechG300Driver::new())),
        "logitech_g600" => Some(Box::new(logitech_g600::LG600Driver::new())),
        "marsgaming" => Some(Box::new(marsgaming::MarsGamingDriver::new())),
        "openinput" => Some(Box::new(openinput::OpenInputDriver::new())),
        "roccat" | "roccat-kone-pure" | "roccat-kone-emp" => {
            Some(Box::new(roccat::RoccatDriver::new(driver_name)))
        }
        "sinowealth" => Some(Box::new(sinowealth::SinowealthDriver::new())),
        "sinowealth-nubwo" => Some(Box::new(sinowealth_nubwo::SinowealthNubwoDriver::new())),
        "steelseries" => Some(Box::new(steelseries::SteelseriesDriver::new())),
        _ => {
            warn!("Unknown driver: {driver_name}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /* Build a DeviceIo over one end of a socketpair; the other end acts
     * as a scripted fake device.  Sockets are pollable and support
     * O_NONBLOCK, so they exercise the exact same AsyncFd paths as a
     * hidraw node. */
    fn fake_device() -> (DeviceIo, UnixStream) {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        ours.set_nonblocking(true).expect("set_nonblocking");
        let file = std::fs::File::from(std::os::unix::io::OwnedFd::from(ours));
        let io = DeviceIo::from_std(file, std::path::PathBuf::from("/dev/fake-hidraw"))
            .expect("from_std");
        (io, theirs)
    }

    #[tokio::test]
    async fn cancelled_read_loses_no_data() {
        /* Regression test for the tokio::fs::File flaw: a timed-out read
         * must not wedge the handle or discard data that arrives later. */
        let (mut io, mut peer) = fake_device();
        let mut buf = [0u8; 8];

        /* 1. Read with nothing queued: times out cleanly. */
        let res =
            tokio::time::timeout(Duration::from_millis(50), io.read_report(&mut buf)).await;
        assert!(res.is_err(), "read should time out with a silent peer");

        /* 2. A write immediately after the cancelled read must not stall. */
        tokio::time::timeout(Duration::from_millis(100), io.write_report(&[0x10, 0x01]))
            .await
            .expect("write must not block after a cancelled read")
            .expect("write should succeed");

        /* 3. Data sent after the cancellation is fully received. */
        use std::io::Write as _;
        peer.write_all(&[0xAA, 0xBB, 0xCC]).expect("peer write");
        let n = tokio::time::timeout(Duration::from_millis(200), io.read_report(&mut buf))
            .await
            .expect("read must complete once data is queued")
            .expect("read should succeed");
        assert_eq!(&buf[..n], &[0xAA, 0xBB, 0xCC]);
    }

    #[tokio::test]
    async fn request_survives_long_silence_before_reply() {
        /* Regression test for the removed SINGLE_READ_TIMEOUT: a reply
         * arriving after >500ms of silence must still match within the
         * 2-second attempt deadline. */
        let (mut io, peer) = fake_device();

        let responder = tokio::task::spawn_blocking(move || {
            let mut peer = peer;
            use std::io::{Read as _, Write as _};
            let mut req = [0u8; 7];
            peer.read_exact(&mut req).expect("peer read");
            std::thread::sleep(Duration::from_millis(800));
            peer.write_all(&[0x10, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06])
                .expect("peer write");
            peer /* keep the socket open until the test ends */
        });

        let report = build_test_short_report();
        let result = io
            .request(&report, 7, 1, |buf| {
                (buf.first() == Some(&0x10) && buf.get(2) == Some(&0x02)).then_some(buf[6])
            })
            .await
            .expect("late reply must still match");
        assert_eq!(result, 0x06);
        drop(responder);
    }

    #[tokio::test]
    async fn request_routes_noise_and_unmatched_reports() {
        let (mut io, peer) = fake_device();

        let responder = tokio::task::spawn_blocking(move || {
            let mut peer = peer;
            use std::io::{Read as _, Write as _};
            let mut req = [0u8; 7];
            peer.read_exact(&mut req).expect("peer read");
            /* Non-HID++ noise (mouse motion): silently discarded. */
            peer.write_all(&[0x02, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00]).unwrap();
            /* Valid HID++ but unmatched: buffered as pending event. */
            peer.write_all(&[0x10, 0x01, 0x99, 0x00, 0x00, 0x00, 0x00]).unwrap();
            /* The actual reply. */
            peer.write_all(&[0x10, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00]).unwrap();
            peer
        });

        let report = build_test_short_report();
        io.request(&report, 7, 1, |buf| {
            (buf.first() == Some(&0x10) && buf.get(2) == Some(&0x02)).then_some(())
        })
        .await
        .expect("reply must match");

        let events = io.drain_events();
        assert_eq!(events.len(), 1, "only the unmatched HID++ report is buffered");
        assert_eq!(events[0][2], 0x99);
        assert!(io.drain_events().is_empty(), "drain clears the buffer");
        drop(responder);
    }

    #[tokio::test]
    async fn wait_readable_does_not_consume() {
        let (mut io, mut peer) = fake_device();
        use std::io::Write as _;
        peer.write_all(&[0x11, 0x22]).expect("peer write");

        tokio::time::timeout(Duration::from_millis(200), io.wait_readable())
            .await
            .expect("readable within deadline")
            .expect("no poll error");

        /* The queued report must still be there. */
        let mut buf = [0u8; 8];
        let n = io.try_read_report(&mut buf).expect("read ok");
        assert_eq!(n, Some(2));
        assert_eq!(&buf[..2], &[0x11, 0x22]);

        /* And with the queue drained, try_read_report reports EAGAIN. */
        assert_eq!(io.try_read_report(&mut buf).expect("read ok"), None);
    }

    #[tokio::test]
    async fn pending_events_are_capped() {
        let (mut io, _peer) = fake_device();
        for i in 0..(MAX_PENDING_EVENTS + 8) {
            io.push_pending_event(vec![i as u8]);
        }
        let events = io.drain_events();
        assert_eq!(events.len(), MAX_PENDING_EVENTS);
        /* The oldest 8 events were dropped. */
        assert_eq!(events[0], vec![8u8]);
    }

    fn build_test_short_report() -> [u8; 7] {
        [0x10, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00]
    }
}
