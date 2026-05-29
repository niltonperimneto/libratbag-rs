/// G.Skill gaming mouse driver.
///
/// Targets G.Skill Ripjaws mice (MX780 and similar).
/// Protocol features: 5 profiles, up to 5 DPI slots, 10 buttons,
/// 3 LED zones (logo, wheel, tail) plus a DPI LED, and complex macro support.
///
/// # Status
/// **Stub** — protocol constants and data layout are complete, but
/// `probe`/`load_profiles`/`commit` are not yet implemented.
///
/// Reference implementation: `src/driver-gskill.c`.
use anyhow::Result;
use async_trait::async_trait;

use crate::engine::device::DeviceInfo;
use crate::hal::{DeviceDriver, DeviceIo};

/* ------------------------------------------------------------------ */
/* Protocol constants                                                   */
/* ------------------------------------------------------------------ */

const GSKILL_PROFILE_MAX: usize = 5;
const GSKILL_NUM_DPI: usize = 5;
const GSKILL_BUTTON_MAX: usize = 10;
const GSKILL_NUM_LED: usize = 0; /* exposed as capabilities, not counted here */

const GSKILL_MAX_POLLING_RATE: u32 = 1000;

const GSKILL_MIN_DPI: u32 = 100;
const GSKILL_MAX_DPI: u32 = 8200;
const GSKILL_DPI_UNIT: u32 = 50;

/* HID commands */
const GSKILL_GET_CURRENT_PROFILE_NUM: u8 = 0x03;
const GSKILL_GET_SET_MACRO: u8 = 0x04;
const GSKILL_GET_SET_PROFILE: u8 = 0x05;
const GSKILL_GENERAL_CMD: u8 = 0x0c;

/* Report sizes */
const GSKILL_REPORT_SIZE_PROFILE: usize = 644;
const GSKILL_REPORT_SIZE_CMD: usize = 9;
const GSKILL_REPORT_SIZE_MACRO: usize = 2052;

/// Byte offset of the checksum in profile/macro reports.
const GSKILL_CHECKSUM_OFFSET: usize = 3;

/* Command status codes returned by the device */
const GSKILL_CMD_SUCCESS: u8 = 0xb0;
const GSKILL_CMD_IN_PROGRESS: u8 = 0xb1;
const GSKILL_CMD_FAILURE: u8 = 0xb2;
const GSKILL_CMD_IDLE: u8 = 0xb3;

/* LED group indices */
const GSKILL_LED_TYPE_LOGO: usize = 0;
const GSKILL_LED_TYPE_WHEEL: usize = 1;
const GSKILL_LED_TYPE_TAIL: usize = 2;
const GSKILL_LED_TYPE_COUNT: usize = 3;

/* ------------------------------------------------------------------ */
/* LED types                                                            */
/* ------------------------------------------------------------------ */

#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedControlType {
    AllOff = 0x0,
    AllOn = 0x1,
    Breathing = 0x2,
    DpiLedRightCycle = 0x3,
    DpiLedLeftCycle = 0x4,
}

/* ------------------------------------------------------------------ */
/* Button function types                                                */
/* ------------------------------------------------------------------ */

#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonFunctionType {
    Wheel = 0x00,
    Mouse = 0x01,
    Kbd = 0x02,
    Consumer = 0x03,
    Macro = 0x06,
    DpiUp = 0x09,
    DpiDown = 0x0a,
    CycleDpiUp = 0x0b,
    CycleDpiDown = 0x0c,
    ProfileSwitch = 0x0d,
    TemporaryCpiAdjust = 0x15,
    DirectDpiChange = 0x16,
    CycleProfileUp = 0x18,
    CycleProfileDown = 0x19,
    Disable = 0xff,
}

/* ------------------------------------------------------------------ */
/* Keyboard modifier flags                                              */
/* ------------------------------------------------------------------ */

pub const KBD_MOD_CTRL_LEFT: u8 = 1 << 0;
pub const KBD_MOD_SHIFT_LEFT: u8 = 1 << 1;
pub const KBD_MOD_ALT_LEFT: u8 = 1 << 2;
pub const KBD_MOD_SUPER_LEFT: u8 = 1 << 3;
pub const KBD_MOD_CTRL_RIGHT: u8 = 1 << 4;
pub const KBD_MOD_SHIFT_RIGHT: u8 = 1 << 5;
pub const KBD_MOD_ALT_RIGHT: u8 = 1 << 6;
pub const KBD_MOD_SUPER_RIGHT: u8 = 1 << 7;

/* ------------------------------------------------------------------ */
/* Data structures                                                      */
/* ------------------------------------------------------------------ */

/// Raw DPI level entry (2 bytes, one per X/Y axis).
#[derive(Debug, Default, Clone, Copy)]
pub struct RawDpiLevel {
    pub x: u8,
    pub y: u8,
}

/// RGB color entry.
#[derive(Debug, Default, Clone, Copy)]
pub struct LedColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

/// Single LED values (brightness + color).
#[derive(Debug, Default, Clone, Copy)]
pub struct LedValues {
    pub brightness: u8,
    pub color: LedColor,
}

/// A button configuration entry (6 bytes, packed).
#[derive(Debug, Default, Clone, Copy)]
pub struct ButtonCfg {
    pub function_type: u8,
    /// Parameter bytes (meaning depends on `function_type`).
    pub params: [u8; 4],
}

/* ------------------------------------------------------------------ */
/* Macro execution methods                                              */
/* ------------------------------------------------------------------ */

#[repr(u8)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroExecMethod {
    ButtonRelease = 0x1,
    ButtonPress = 0x5,
    ButtonLoopStart = 0x7,
    ButtonLoopEnd = 0x0,
}

/* ------------------------------------------------------------------ */
/* Cached hardware state                                                */
/* ------------------------------------------------------------------ */

#[derive(Debug)]
struct GskillData {
    /// Raw profile reports read from hardware. `None` = not yet loaded.
    profiles: [Option<Box<[u8; GSKILL_REPORT_SIZE_PROFILE]>>; GSKILL_PROFILE_MAX],
    active_profile: u8,
}

/* ------------------------------------------------------------------ */
/* Driver                                                               */
/* ------------------------------------------------------------------ */

pub struct GskillDriver {
    data: Option<GskillData>,
}

impl GskillDriver {
    pub fn new() -> Self {
        Self { data: None }
    }
}

#[async_trait]
impl DeviceDriver for GskillDriver {
    fn name(&self) -> &str {
        "G.Skill"
    }

    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()> {
        /* Query current profile number to confirm device presence. */
        let mut cmd = [0u8; GSKILL_REPORT_SIZE_CMD];
        cmd[0] = GSKILL_GET_CURRENT_PROFILE_NUM;
        io.get_feature_report(&mut cmd)
            .map_err(anyhow::Error::from)?;

        let status = cmd[1];
        if status != GSKILL_CMD_SUCCESS && status != GSKILL_CMD_IDLE {
            anyhow::bail!("G.Skill probe: unexpected status byte {status:#04x}");
        }

        self.data = Some(GskillData {
            profiles: Default::default(),
            active_profile: cmd[2] & 0x0f,
        });

        // TODO: read all profiles using GSKILL_GET_SET_PROFILE.
        anyhow::bail!("G.Skill driver: load_profiles not yet implemented in the Rust port");
    }

    async fn load_profiles(&mut self, _io: &mut DeviceIo, _info: &mut DeviceInfo) -> Result<()> {
        // TODO: parse cached profile reports and fill info.profiles.
        anyhow::bail!("G.Skill driver: load_profiles not yet implemented in the Rust port");
    }

    async fn commit(&mut self, _io: &mut DeviceIo, _info: &DeviceInfo) -> Result<()> {
        // TODO: write dirty profiles back using GSKILL_GET_SET_PROFILE.
        anyhow::bail!("G.Skill driver: commit not yet implemented in the Rust port");
    }
}

/* ------------------------------------------------------------------ */
/* Helpers                                                              */
/* ------------------------------------------------------------------ */

/// Convert a raw DPI pair to actual DPI values (X, Y).
///
/// Raw = `dpi / GSKILL_DPI_UNIT - 1`.
#[allow(dead_code)]
pub fn raw_to_dpi(raw: RawDpiLevel) -> (u32, u32) {
    let to_dpi = |r: u8| -> u32 { (u32::from(r) + 1) * GSKILL_DPI_UNIT };
    (to_dpi(raw.x), to_dpi(raw.y))
}

/// Encode a DPI value to the 1-byte hardware representation.
#[allow(dead_code)]
pub fn dpi_to_raw(dpi: u32) -> Option<u8> {
    if dpi < GSKILL_MIN_DPI || dpi > GSKILL_MAX_DPI || dpi % GSKILL_DPI_UNIT != 0 {
        return None;
    }
    u8::try_from((dpi / GSKILL_DPI_UNIT).saturating_sub(1)).ok()
}

/// Compute the one-byte XOR checksum expected at `GSKILL_CHECKSUM_OFFSET`.
///
/// The checksum covers bytes 4..end of the report.
#[allow(dead_code)]
pub fn compute_checksum(report: &[u8]) -> u8 {
    report[4..].iter().fold(0u8, |acc, &b| acc ^ b)
}
