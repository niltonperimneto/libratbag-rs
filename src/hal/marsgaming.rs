/// MarsGaming MM4 gaming mouse driver.
///
/// Targets MarsGaming MM4 mice using the proprietary MarsGaming HID protocol.
/// Features: 5 profiles, up to 5 DPI resolutions per profile, 19 buttons, 1 LED zone.
///
/// # Status
/// **Stub** — protocol constants and data layout are complete, but
/// `probe`/`load_profiles`/`commit` are not yet implemented.
///
/// Reference implementation: `src/driver-marsgaming/`.
use anyhow::Result;
use async_trait::async_trait;

use crate::engine::device::DeviceInfo;
use crate::hal::{DeviceDriver, DeviceIo};

/* ------------------------------------------------------------------ */
/* Protocol constants                                                   */
/* ------------------------------------------------------------------ */

const NUM_PROFILES: usize = 5;
const NUM_RESOLUTIONS_PER_PROFILE: usize = 5;
const NUM_BUTTONS: usize = 19;
const NUM_LED: usize = 1;

const RES_MIN: u32 = 50;    /* DPI */
const RES_MAX: u32 = 16400; /* DPI */
const RES_SCALING: u32 = 50;

/* ------------------------------------------------------------------ */
/* Report types                                                         */
/* ------------------------------------------------------------------ */

#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportType {
    Unknown1 = 0x01,
    Write = 0x02,
    Read = 0x03,
    Unknown4 = 0x04,
    Unknown6 = 0x06,
}

/* ------------------------------------------------------------------ */
/* Resolution report (64 bytes)                                         */
/* ------------------------------------------------------------------ */

/// A single DPI resolution slot (7 bytes packed).
#[derive(Debug, Default, Clone, Copy)]
pub struct ResolutionInfo {
    pub enabled: bool,
    pub x_res: u16,
    pub y_res: u16,
    /// 4-bit LED bitset: resolution 0 → 0b0000, resolution 1 → 0b0001, etc.
    pub led_bitset: u8,
    pub _zeros: [u8; 2],
}

/// Full resolution read/write report (must be 64 bytes).
#[derive(Debug, Default, Clone)]
pub struct ResolutionReport {
    pub usb_report_id: u8,
    /// Report type field (see `ReportType`).
    pub report_type: u8,
    pub unknown_2: u8, /* 0x4f */
    pub profile_id: u8,
    pub unknown_4: u8, /* 0x2a */
    pub unknown_5: u8,
    pub unknown_6: u8, /* 0x00 from device | 0xfa from host */
    pub unknown_7: u8,
    pub count_resolutions: u8,
    pub current_resolution: u8,
    pub resolutions: [ResolutionInfo; 6],
    pub padding: [u8; 6],
}

/* ------------------------------------------------------------------ */
/* Button report (1024 bytes)                                           */
/* ------------------------------------------------------------------ */

/// A single button assignment (4 bytes packed).
#[derive(Debug, Default, Clone, Copy)]
pub struct ButtonInfo {
    pub function_type: u8,
    pub params: [u8; 3],
}

/// Full button read/write report (must be 1024 bytes).
#[derive(Debug)]
pub struct ButtonReport {
    pub usb_report_id: u8,
    pub report_type: u8,
    pub unknown_2: u8, /* 0x90 */
    pub profile_id: u8,
    pub unknown_4: u8, /* 0x4d */
    pub unknown_5: u8,
    pub unknown_6: u8,
    pub unknown_7: u8,
    pub button_count: u8,
    pub buttons: Box<[ButtonInfo; 253]>,
    pub padding: [u8; 3],
}

impl Default for ButtonReport {
    fn default() -> Self {
        Self {
            usb_report_id: 0,
            report_type: 0,
            unknown_2: 0,
            profile_id: 0,
            unknown_4: 0,
            unknown_5: 0,
            unknown_6: 0,
            unknown_7: 0,
            button_count: 0,
            buttons: Box::new([ButtonInfo::default(); 253]),
            padding: [0u8; 3],
        }
    }
}

/* ------------------------------------------------------------------ */
/* LED report (16 bytes)                                                */
/* ------------------------------------------------------------------ */

/// LED color mode.
#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LedMode {
    #[default]
    Off = 0x00,
    Static = 0x01,
    Breathing = 0x02,
    Rainbow = 0x03,
}

/// LED state (6 bytes payload within the LED report).
#[derive(Debug, Default, Clone, Copy)]
pub struct LedState {
    pub mode: u8,
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub speed: u8,
    pub brightness: u8,
}

/// Full LED read/write report (must be 16 bytes).
#[derive(Debug, Default, Clone)]
pub struct LedReport {
    pub usb_report_id: u8,
    pub report_type: u8,
    pub unknown_2: u8, /* 0xf1 */
    pub profile_id: u8,
    pub unknown_4: u8, /* 0x06 */
    pub unknown_5: u8,
    pub unknown_6: u8, /* 0xfa */
    pub unknown_7: u8, /* 0xfa */
    pub led: LedState,
    pub unknown_13: u8,
    pub unknown_14: u8,
    pub unknown_15: u8,
}

/* ------------------------------------------------------------------ */
/* Per-profile cached data                                              */
/* ------------------------------------------------------------------ */

#[derive(Debug, Default)]
struct ProfileData {
    buttons: ButtonReport,
    resolutions: ResolutionReport,
    led: LedReport,
}

/* ------------------------------------------------------------------ */
/* Device-level cached state                                            */
/* ------------------------------------------------------------------ */

#[derive(Debug)]
struct MarsData {
    profiles: Vec<ProfileData>,
    active_profile: u8,
}

/* ------------------------------------------------------------------ */
/* Driver                                                               */
/* ------------------------------------------------------------------ */

pub struct MarsGamingDriver {
    data: Option<MarsData>,
}

impl MarsGamingDriver {
    pub fn new() -> Self {
        Self { data: None }
    }
}

#[async_trait]
impl DeviceDriver for MarsGamingDriver {
    fn name(&self) -> &str {
        "MarsGaming MM4"
    }

    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()> {
        /* Send a READ resolution request for profile 0 to confirm device presence. */
        let mut buf = [0u8; 64];
        buf[0] = 0x01; /* USB report ID */
        buf[1] = ReportType::Read as u8;
        buf[2] = 0x4f;
        buf[3] = 0x00; /* profile 0 */
        buf[4] = 0x2a;
        buf[6] = 0xfa;
        buf[7] = 0xfa;

        io.write_report(&buf).await?;
        io.read_report(&mut buf).await?;

        self.data = Some(MarsData {
            profiles: (0..NUM_PROFILES).map(|_| ProfileData::default()).collect(),
            active_profile: 0,
        });

        // TODO: read all profiles (buttons, resolutions, LEDs) for all 5 profiles.
        anyhow::bail!(
            "MarsGaming driver: load_profiles not yet implemented in the Rust port"
        );
    }

    async fn load_profiles(&mut self, _io: &mut DeviceIo, _info: &mut DeviceInfo) -> Result<()> {
        // TODO: parse cached profile data and fill info.profiles.
        anyhow::bail!(
            "MarsGaming driver: load_profiles not yet implemented in the Rust port"
        );
    }

    async fn commit(&mut self, _io: &mut DeviceIo, _info: &DeviceInfo) -> Result<()> {
        // TODO: write dirty profiles back using WRITE report type.
        anyhow::bail!(
            "MarsGaming driver: commit not yet implemented in the Rust port"
        );
    }
}

/* ------------------------------------------------------------------ */
/* Helpers                                                              */
/* ------------------------------------------------------------------ */

/// Encode a DPI value to its 16-bit hardware representation.
///
/// The device stores DPI as `dpi / RES_SCALING`.
#[allow(dead_code)]
pub fn dpi_to_raw(dpi: u32) -> Option<u16> {
    if dpi < RES_MIN || dpi > RES_MAX || dpi % RES_SCALING != 0 {
        return None;
    }
    u16::try_from(dpi / RES_SCALING).ok()
}

/// Decode the raw 16-bit DPI value to Hz.
#[allow(dead_code)]
pub fn raw_to_dpi(raw: u16) -> u32 {
    u32::from(raw) * RES_SCALING
}
