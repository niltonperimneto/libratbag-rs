/// SinoWealth Nubwo gaming mouse driver.
///
/// Covers Nubwo mice that use the simplified SinoWealth variant protocol.
/// Distinct from the standard SinoWealth driver — uses different report IDs
/// and a fixed command structure rather than the extended config reports.
///
/// # Status
/// **Stub** — protocol constants and data layout are complete, but
/// `probe`/`load_profiles`/`commit` are not yet implemented.
///
/// Reference implementation: `src/driver-sinowealth-nubwo.c`.
use anyhow::Result;
use async_trait::async_trait;

use crate::engine::device::DeviceInfo;
use crate::hal::{DeviceDriver, DeviceIo};

/* ------------------------------------------------------------------ */
/* Protocol constants                                                   */
/* ------------------------------------------------------------------ */

/// HID report ID for performance commands (rate, DPI).
const REPORTID_PERF_CMD: u8 = 0x02;
/// HID report ID for aesthetic commands (LED).
const REPORTID_AESTHETIC_CMD: u8 = 0x03;
/// HID report ID for the firmware version query.
const REPORTID_GET_FIRMWARE: u8 = 0x04;

/// Size of the firmware version response (bytes).
const GET_FIRMWARE_MSGSIZE: usize = 256;
/// Byte offset where the firmware string starts.
const GET_FIRMWARE_MSGOFFSET: usize = 48;

/// Size of performance command reports.
const PERF_CMD_MSGSIZE: usize = 16;

const NUM_PROFILES: usize = 1;
const NUM_RESOLUTIONS: usize = 1;
const NUM_BUTTONS: usize = 0; /* macros not implemented */
const NUM_LEDS: usize = 1;

/// Magic SET_FEATURE that must precede a firmware version query.
const PREFIRMWARE_QUERY: [u8; 16] = [
    0x02, 0x01, 0x49, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

/// Valid polling rates (Hz).
const REPORT_RATES: &[u32] = &[125, 250, 333, 500, 1000];
/// Encoded polling-rate bytes (same order as `REPORT_RATES`).
const REPORT_RATES_ENCODED: &[u8] = &[0x08, 0x04, 0x03, 0x02, 0x01];
/// Template for the polling-rate SET_FEATURE command.
const REPORT_RATE_CMD: [u8; 8] = [0x02, 0x06, 0xbb, 0xaa, 0x28, 0x00, 0x01, 0x00];

/// Valid DPI values (cps).
const DPI_LIST: &[u32] = &[1000, 2000, 3000, 5000, 15_000];
/// Encoded DPI bytes (same order as `DPI_LIST`).
const DPI_ENCODED: &[u8] = &[0x04, 0x03, 0x02, 0x01, 0x00];
/// Template for the DPI SET_FEATURE command.
const DPI_CMD: [u8; 8] = [0x02, 0x06, 0xbb, 0xaa, 0x32, 0x00, 0x01, 0x00];

/* ------------------------------------------------------------------ */
/* LED color modes                                                      */
/* ------------------------------------------------------------------ */

#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Off = 0x00,
    On = 0x01,
    Breathing = 0x02,
    ColorShift = 0x03,
    Spectrum = 0x04,
    Marquee = 0x05,
}

/* ------------------------------------------------------------------ */
/* Aesthetic (LED) report layout (14 bytes)                             */
/* ------------------------------------------------------------------ */

/// Full aesthetic SET_FEATURE report.
#[derive(Debug, Default, Clone)]
pub struct AestheticReport {
    /// Report ID = `REPORTID_AESTHETIC_CMD` (0x03).
    pub report_id: u8,
    pub cmd: [u8; 7],
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub color_mode: u8,
    pub pad_zero: u8,
    /// Brightness: 0x01 (low) - 0x03 (high).
    pub brightness: u8,
    /// Speed / tempo: 0x01 (fast), 0x03 (slow), 0x05 (very slow).
    pub tempo: u8,
}

/* ------------------------------------------------------------------ */
/* Cached state                                                         */
/* ------------------------------------------------------------------ */

#[derive(Debug)]
struct NubwoData {
    firmware_string: String,
    current_dpi_encoded: u8,
    current_rate_encoded: u8,
}

/* ------------------------------------------------------------------ */
/* Driver                                                               */
/* ------------------------------------------------------------------ */

pub struct SinowealthNubwoDriver {
    data: Option<NubwoData>,
}

impl SinowealthNubwoDriver {
    pub fn new() -> Self {
        Self { data: None }
    }
}

#[async_trait]
impl DeviceDriver for SinowealthNubwoDriver {
    fn name(&self) -> &str {
        "SinoWealth-Nubwo"
    }

    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()> {
        /* Send magic pre-query to enable firmware report. */
        io.set_feature_report(&PREFIRMWARE_QUERY)
            .map_err(anyhow::Error::from)?;

        let mut buf = [0u8; GET_FIRMWARE_MSGSIZE];
        buf[0] = REPORTID_GET_FIRMWARE;
        io.get_feature_report(&mut buf)
            .map_err(anyhow::Error::from)?;

        let fw_bytes = &buf[GET_FIRMWARE_MSGOFFSET..];
        let fw_len = fw_bytes.iter().position(|&b| b == 0).unwrap_or(fw_bytes.len());
        let firmware_string = String::from_utf8_lossy(&fw_bytes[..fw_len]).into_owned();

        self.data = Some(NubwoData {
            firmware_string,
            current_dpi_encoded: DPI_ENCODED[0],
            current_rate_encoded: REPORT_RATES_ENCODED[REPORT_RATES.len() - 1],
        });

        // TODO: read current DPI and rate from the device.
        anyhow::bail!(
            "SinoWealth-Nubwo driver: load_profiles not yet implemented in the Rust port"
        );
    }

    async fn load_profiles(&mut self, _io: &mut DeviceIo, _info: &mut DeviceInfo) -> Result<()> {
        anyhow::bail!(
            "SinoWealth-Nubwo driver: load_profiles not yet implemented in the Rust port"
        );
    }

    async fn commit(&mut self, _io: &mut DeviceIo, _info: &DeviceInfo) -> Result<()> {
        anyhow::bail!(
            "SinoWealth-Nubwo driver: commit not yet implemented in the Rust port"
        );
    }
}

/* ------------------------------------------------------------------ */
/* Helpers                                                              */
/* ------------------------------------------------------------------ */

/// Encode a DPI value for the command report.
/// Returns `None` if the DPI is not in the supported list.
#[allow(dead_code)]
pub fn encode_dpi(dpi: u32) -> Option<u8> {
    DPI_LIST
        .iter()
        .position(|&d| d == dpi)
        .map(|i| DPI_ENCODED[i])
}

/// Encode a polling rate for the command report.
#[allow(dead_code)]
pub fn encode_rate(rate: u32) -> Option<u8> {
    REPORT_RATES
        .iter()
        .position(|&r| r == rate)
        .map(|i| REPORT_RATES_ENCODED[i])
}

/// Build the DPI SET_FEATURE command.
#[allow(dead_code)]
pub fn build_dpi_cmd(encoded: u8) -> [u8; 8] {
    let mut cmd = DPI_CMD;
    cmd[6] = encoded;
    cmd
}

/// Build the polling-rate SET_FEATURE command.
#[allow(dead_code)]
pub fn build_rate_cmd(encoded: u8) -> [u8; 8] {
    let mut cmd = REPORT_RATE_CMD;
    cmd[6] = encoded;
    cmd
}
