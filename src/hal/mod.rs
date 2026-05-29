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
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
const READ_TIMEOUT_PER_ATTEMPT: Duration = Duration::from_millis(2000);

/* Timeout for each individual read syscall within the loop.      */
const SINGLE_READ_TIMEOUT: Duration = Duration::from_millis(500);

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

/* Async wrapper around a `/dev/hidraw` file descriptor. */
/*                                                       */
/* All hardware I/O goes through this struct so that     */
/* drivers never touch raw file handles directly.        */
pub struct DeviceIo {
    file: tokio::fs::File,
    path: std::path::PathBuf,
    /* Reports seen during `request()` that were valid HID++ but did not
     * match the pending command.  These are unsolicited hardware events
     * (e.g. profile-switch notifications) that the actor should forward
     * to `DeviceDriver::handle_event` after each I/O batch. */
    pending_events: Vec<Vec<u8>>,
}

impl DeviceIo {
    /* Open the hidraw device node at `path`. */
    pub async fn open(path: &Path) -> Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .await
            .with_context(|| format!("Failed to open hidraw device {}", path.display()))?;

        Ok(Self {
            file,
            path: path.to_path_buf(),
            pending_events: Vec::new(),
        })
    }

    /* Return the path of the underlying hidraw device node. */
    pub fn path(&self) -> &Path {
        &self.path
    }

    /* Write a raw HID report to the device. */
    pub async fn write_report(&mut self, buf: &[u8]) -> Result<()> {
        self.file
            .write_all(buf)
            .await
            .with_context(|| format!("Write failed on {}", self.path.display()))?;
        debug!("TX {} bytes: {:02x?}", buf.len(), buf);
        Ok(())
    }

    /* Read a single HID report from the device (blocks until data arrives). */
    pub async fn read_report(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = self
            .file
            .read(buf)
            .await
            .with_context(|| format!("Read failed on {}", self.path.display()))?;
        debug!("RX {} bytes: {:02x?}", n, &buf[..n]);
        Ok(n)
    }

    /* Get a HID feature report using the `HIDIOCGFEATURE` ioctl.  */
    /*                                                             */
    /* `buf[0]` must contain the report ID before calling; the     */
    /* kernel fills the remaining bytes with the report data and   */
    /* returns the total number of bytes written.                  */
    pub fn get_feature_report(&self, buf: &mut [u8]) -> Result<usize, DriverError> {
        let fd = self.file.as_raw_fd();
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
        let fd = self.file.as_raw_fd();
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
    ) -> Result<T>
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
            }
            .into());
        }

        for attempt in 1..=max_attempts {
            self.write_report(report).await?;

            let deadline = tokio::time::Instant::now() + READ_TIMEOUT_PER_ATTEMPT;
            let mut backing = [0u8; MAX_HID_REPORT];
            let buf = &mut backing[..report_size];

            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    trace!("Read deadline expired on attempt {attempt}");
                    break;
                }

                /* Use the shorter of the remaining budget and the     */
                /* per-read timeout so we don't block forever on a     */
                /* single read if the device stops sending reports.     */
                let read_timeout = remaining.min(SINGLE_READ_TIMEOUT);

                match tokio::time::timeout(read_timeout, self.read_report(buf)).await {
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
                        self.pending_events.push(buf[..n].to_vec());
                    }
                    Ok(Err(e)) => {
                        warn!("Read error on attempt {attempt}: {e}");
                        break;
                    }
                    Err(_elapsed) => {
                        /* Single-read timeout: no more data coming,   */
                        /* break to retry with a fresh write.          */
                        trace!("Timeout on attempt {attempt}");
                        break;
                    }
                }
            }
        }

        Err(DriverError::Timeout {
            attempts: max_attempts,
        }
        .into())
    }

    /* Drain all unsolicited HID++ events that were buffered during
     * `request()` calls.  The actor calls this after each I/O batch
     * and forwards the reports to `DeviceDriver::handle_event`. */
    pub fn drain_events(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_events)
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
