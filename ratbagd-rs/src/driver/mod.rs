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
use tracing::{debug, warn};

use crate::device::DeviceInfo;

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
}

/* Maximum HID report size.                                        */
/*                                                                 */
/* Roccat macros are the largest at 2082 bytes. We use 4096 as    */
/* a safe ceiling covering any current and future HID report.     */
#[allow(dead_code)]
const MAX_REPORT_LEN: usize = 4096;

/* Timeout per individual read attempt */
const READ_TIMEOUT: Duration = Duration::from_millis(500);

/* Maximum number of reads to attempt per single request retry */
const MAX_READS_PER_ATTEMPT: usize = 10;

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
        })
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
    /* The `matcher` closure receives each incoming report and     */
    /* returns `Some(T)` when the expected response has arrived,   */
    /* or `None` to keep waiting. Retries up to `max_attempts`    */
    /* times. Fails with `DriverError::Timeout` when exhausted.   */
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
        for attempt in 1..=max_attempts {
            self.write_report(report).await?;

            let mut buf = vec![0u8; report_size];
            for _ in 0..MAX_READS_PER_ATTEMPT {
                match tokio::time::timeout(READ_TIMEOUT, self.read_report(&mut buf)).await {
                    Ok(Ok(n)) => {
                        if let Some(result) = matcher(&buf[..n]) {
                            return Ok(result);
                        }
                    }
                    Ok(Err(e)) => {
                        warn!("Read error on attempt {attempt}: {e}");
                        break;
                    }
                    Err(_elapsed) => {
                        debug!("Timeout on attempt {attempt}");
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
        "sinowealth" => Some(Box::new(sinowealth::SinowealhDriver::new())),
        "sinowealth-nubwo" => Some(Box::new(sinowealth_nubwo::SinowealhNubwoDriver::new())),
        "steelseries" => Some(Box::new(steelseries::SteelseriesDriver::new())),
        _ => {
            warn!("Unknown driver: {driver_name}");
            None
        }
    }
}
