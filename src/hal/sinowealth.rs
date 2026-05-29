/// SinoWealth-based gaming mouse driver.
///
/// Covers mice using the SinoWealth HID protocol: Glorious Model O/O-,
/// G-Wolves Skoll, Genesis Xenon 770, DreamMachines DM5, and similar devices.
///
/// Reference implementation: C libratbag `driver-sinowealth.c`.
use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{debug, warn};

use crate::engine::device::{
    ActionType, ButtonInfo, Color, DeviceInfo, Dpi, LedInfo, LedMode, ProfileInfo, RgbColor,
};
use crate::engine::device_database::SinowealthLedType;
use crate::hal::{DeviceDriver, DeviceIo};

/* ------------------------------------------------------------------ */
/* Report IDs                                                           */
/* ------------------------------------------------------------------ */

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportId {
    Config = 0x04,
    Cmd = 0x05,
    ConfigLong = 0x06,
}

/* ------------------------------------------------------------------ */
/* Command IDs                                                          */
/* ------------------------------------------------------------------ */

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandId {
    FirmwareVersion = 0x01,
    Profile = 0x02,
    GetConfig = 0x11,
    GetButtons = 0x12,
    Debounce = 0x1a,
    LongAngleSnappingAndLod = 0x1b,
    GetConfig2 = 0x21,
    GetButtons2 = 0x22,
    Macro = 0x30,
    GetConfig3 = 0x31,
    GetButtons3 = 0x32,
    Dfu = 0x75,
}

/* ------------------------------------------------------------------ */
/* Protocol constants                                                   */
/* ------------------------------------------------------------------ */

pub const SINOWEALTH_CMD_SIZE: usize = 6;
pub const SINOWEALTH_CONFIG_REPORT_SIZE: usize = 520;
pub const SINOWEALTH_CONFIG_SIZE_MAX: usize = 167;
pub const SINOWEALTH_CONFIG_SIZE_MIN: usize = 123;
pub const SINOWEALTH_BUTTON_SIZE: usize = 88;
pub const SINOWEALTH_BUTTON_REPORT_SIZE: usize = 520;
pub const SINOWEALTH_MACRO_SIZE: usize = 515;

pub const SINOWEALTH_DPI_MIN: u32 = 100;
pub const SINOWEALTH_DPI_STEP: u32 = 100;

pub const SINOWEALTH_NUM_DPIS: usize = 8;
pub const SINOWEALTH_NUM_PROFILES_MAX: usize = 3;
pub const SINOWEALTH_NUM_BUTTONS: usize = 20;
pub const SINOWEALTH_MACRO_LENGTH_MAX: usize = 168;
pub const SINOWEALTH_MACRO_EVENT_SIZE: usize = 3;

pub const SINOWEALTH_DEBOUNCE_TIMES: &[u32] = &[4, 6, 8, 10, 12, 14, 16];
pub const SINOWEALTH_REPORT_RATES: &[u32] = &[125, 250, 500, 1000];

/// Bit 3 of the config byte: independent X/Y DPI.
pub const SINOWEALTH_XY_INDEPENDENT: u8 = 0b0000_1000;

/* Config report byte offsets (0-indexed within the 520-byte buffer).
 * Byte 0 is the HID report ID. */
mod offset {
    pub const CONFIG_FLAGS: usize = 4;
    pub const DPI_COUNT: usize = 6;
    pub const DPI_SLOTS: usize = 7;
    pub const DPI_ACTIVE_COLOR: usize = 23;
    pub const DPI_COLORS: usize = 24;
    pub const REPORT_RATE: usize = 72;
    pub const LED_EFFECT: usize = 77;
    pub const LED_COLOR: usize = 78;
    pub const LED_SPEED: usize = 81;
    pub const LED_BRIGHTNESS: usize = 83;
}

/* Button report: 4 bytes per button entry, starting at offset 4. */
const BUTTON_ENTRY_OFFSET: usize = 4;
const BUTTON_ENTRY_SIZE: usize = 4;

/* ------------------------------------------------------------------ */
/* Sensor IDs                                                           */
/* ------------------------------------------------------------------ */

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensor {
    Pmw3360 = 0x06,
    Pmw3212 = 0x08,
    Pmw3327 = 0x0e,
    Pmw3389 = 0x0f,
}

impl Sensor {
    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "PMW3360" => Some(Sensor::Pmw3360),
            "PMW3212" => Some(Sensor::Pmw3212),
            "PMW3327" => Some(Sensor::Pmw3327),
            "PMW3389" => Some(Sensor::Pmw3389),
            _ => None,
        }
    }

    /// PMW3389: `raw * 100`, all others: `(raw + 1) * 100`.
    pub fn raw_to_dpi(self, raw: u8) -> u32 {
        match self {
            Sensor::Pmw3389 => u32::from(raw) * SINOWEALTH_DPI_STEP,
            _ => (u32::from(raw) + 1) * SINOWEALTH_DPI_STEP,
        }
    }

    pub fn dpi_to_raw(self, dpi: u32) -> Option<u8> {
        if dpi < SINOWEALTH_DPI_MIN {
            return None;
        }
        let raw = match self {
            Sensor::Pmw3389 => dpi / SINOWEALTH_DPI_STEP,
            _ => (dpi / SINOWEALTH_DPI_STEP).saturating_sub(1),
        };
        u8::try_from(raw).ok()
    }

    pub fn max_dpi(self) -> u32 {
        match self {
            Sensor::Pmw3327 => 10200,
            Sensor::Pmw3212 => 7200,
            Sensor::Pmw3360 => 12000,
            Sensor::Pmw3389 => 16000,
        }
    }
}

/* ------------------------------------------------------------------ */
/* RGB effect modes                                                     */
/* ------------------------------------------------------------------ */

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RgbEffect {
    Off = 0x00,
    Glorious = 0x01,
    Single = 0x02,
    Breathing7 = 0x03,
    Tail = 0x04,
    Breathing = 0x05,
    Constant = 0x06,
    Rave = 0x07,
    Random = 0x08,
    Wave = 0x09,
    Breathing1 = 0x0a,
    NotSupported = 0xff,
}

impl RgbEffect {
    fn from_byte(b: u8) -> Self {
        match b {
            0x00 => RgbEffect::Off,
            0x01 => RgbEffect::Glorious,
            0x02 => RgbEffect::Single,
            0x03 => RgbEffect::Breathing7,
            0x04 => RgbEffect::Tail,
            0x05 => RgbEffect::Breathing,
            0x06 => RgbEffect::Constant,
            0x07 => RgbEffect::Rave,
            0x08 => RgbEffect::Random,
            0x09 => RgbEffect::Wave,
            0x0a => RgbEffect::Breathing1,
            0xff => RgbEffect::NotSupported,
            _ => RgbEffect::Off,
        }
    }
}

/* ------------------------------------------------------------------ */
/* LED type (mirrors SinowealthLedType from device_database)            */
/* ------------------------------------------------------------------ */

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LedType {
    None,
    Rgb,
    Rbg,
}

impl From<SinowealthLedType> for LedType {
    fn from(lt: SinowealthLedType) -> Self {
        match lt {
            SinowealthLedType::None => LedType::None,
            SinowealthLedType::Rgb => LedType::Rgb,
            SinowealthLedType::Rbg => LedType::Rbg,
        }
    }
}

/* ------------------------------------------------------------------ */
/* Button action types in the hardware protocol                         */
/* ------------------------------------------------------------------ */

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
enum HwButtonType {
    MouseButton = 0x01,
    Keyboard = 0x10,
    ModifierKey = 0x11,
    Special = 0x12,
    Disabled = 0x20,
    Macro = 0x40,
}

impl HwButtonType {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(HwButtonType::MouseButton),
            0x10 => Some(HwButtonType::Keyboard),
            0x11 => Some(HwButtonType::ModifierKey),
            0x12 => Some(HwButtonType::Special),
            0x20 => Some(HwButtonType::Disabled),
            0x40 => Some(HwButtonType::Macro),
            _ => None,
        }
    }
}

/* ------------------------------------------------------------------ */
/* Hardware special-action mapping table                                */
/* ------------------------------------------------------------------ */

use crate::engine::device::special_action as sa;

/// Map SinoWealth hardware special codes to ratbag special action constants.
fn hw_special_to_ratbag(code: u8) -> u32 {
    match code {
        0x01 => sa::WHEEL_UP,
        0x02 => sa::WHEEL_DOWN,
        0x03 => sa::RESOLUTION_CYCLE_UP,
        0x04 => sa::RESOLUTION_UP,
        0x05 => sa::RESOLUTION_DOWN,
        0x06 => sa::PROFILE_CYCLE_UP,
        0x07 => sa::PROFILE_UP,
        0x08 => sa::PROFILE_DOWN,
        0x09 => sa::DOUBLECLICK,
        0x0a => sa::WHEEL_LEFT,
        0x0b => sa::WHEEL_RIGHT,
        0x0c => sa::RESOLUTION_ALTERNATE,
        0x0d => sa::RESOLUTION_DEFAULT,
        0x0e => sa::BATTERY_LEVEL,
        0x0f => sa::SECOND_MODE,
        0x10 => sa::RESOLUTION_CYCLE_DOWN,
        _ => sa::UNKNOWN,
    }
}

/// Inverse: ratbag special action constant to hardware code.
fn ratbag_special_to_hw(action: u32) -> u8 {
    match action {
        x if x == sa::WHEEL_UP => 0x01,
        x if x == sa::WHEEL_DOWN => 0x02,
        x if x == sa::RESOLUTION_CYCLE_UP => 0x03,
        x if x == sa::RESOLUTION_UP => 0x04,
        x if x == sa::RESOLUTION_DOWN => 0x05,
        x if x == sa::PROFILE_CYCLE_UP => 0x06,
        x if x == sa::PROFILE_UP => 0x07,
        x if x == sa::PROFILE_DOWN => 0x08,
        x if x == sa::DOUBLECLICK => 0x09,
        x if x == sa::WHEEL_LEFT => 0x0a,
        x if x == sa::WHEEL_RIGHT => 0x0b,
        x if x == sa::RESOLUTION_ALTERNATE => 0x0c,
        x if x == sa::RESOLUTION_DEFAULT => 0x0d,
        x if x == sa::BATTERY_LEVEL => 0x0e,
        x if x == sa::SECOND_MODE => 0x0f,
        x if x == sa::RESOLUTION_CYCLE_DOWN => 0x10,
        _ => 0x00,
    }
}

/* ------------------------------------------------------------------ */
/* Button encoding / decoding                                           */
/* ------------------------------------------------------------------ */

/// Decode a 4-byte hardware button entry into (ActionType, mapping_value).
fn decode_button(raw: &[u8; 4]) -> (ActionType, u32) {
    match HwButtonType::from_byte(raw[0]) {
        Some(HwButtonType::MouseButton) => {
            let btn = match raw[1] {
                0x01 => 0x110, // BTN_LEFT
                0x02 => 0x111, // BTN_RIGHT
                0x04 => 0x112, // BTN_MIDDLE
                0x08 => 0x113, // BTN_SIDE
                0x10 => 0x114, // BTN_EXTRA
                _ => 0x110,
            };
            (ActionType::Button, btn)
        }
        Some(HwButtonType::Special) => (ActionType::Special, hw_special_to_ratbag(raw[1])),
        Some(HwButtonType::Keyboard) => (ActionType::Key, u32::from(raw[1])),
        Some(HwButtonType::ModifierKey) => {
            // raw[1] = modifier mask, raw[2] = keycode
            // Encode as (modifier << 16) | keycode for round-trip
            let combined = (u32::from(raw[1]) << 16) | u32::from(raw[2]);
            (ActionType::Key, combined)
        }
        Some(HwButtonType::Disabled) => (ActionType::None, 0),
        Some(HwButtonType::Macro) => (ActionType::Macro, u32::from(raw[1])),
        None => {
            warn!("Unknown SinoWealth button type: {:#04x}", raw[0]);
            (ActionType::None, 0)
        }
    }
}

/// Encode an (ActionType, mapping_value) back into 4 hardware bytes.
fn encode_button(action_type: ActionType, value: u32) -> [u8; 4] {
    match action_type {
        ActionType::Button => {
            let hw = match value {
                0x110 => 0x01u8, // BTN_LEFT
                0x111 => 0x02,   // BTN_RIGHT
                0x112 => 0x04,   // BTN_MIDDLE
                0x113 => 0x08,   // BTN_SIDE
                0x114 => 0x10,   // BTN_EXTRA
                _ => 0x01,
            };
            [HwButtonType::MouseButton as u8, hw, 0, 0]
        }
        ActionType::Special => {
            let hw = ratbag_special_to_hw(value);
            [HwButtonType::Special as u8, hw, 0, 0]
        }
        ActionType::Key => {
            if value > 0xFFFF {
                // Modifier+key combo
                let modifier = (value >> 16) as u8;
                let keycode = (value & 0xFF) as u8;
                [HwButtonType::ModifierKey as u8, modifier, keycode, 0]
            } else {
                [HwButtonType::Keyboard as u8, value as u8, 0, 0]
            }
        }
        ActionType::Macro => [HwButtonType::Macro as u8, value as u8, 0, 0],
        ActionType::None | ActionType::Unknown => [HwButtonType::Disabled as u8, 0, 0, 0],
    }
}

/* ------------------------------------------------------------------ */
/* Cached hardware state                                                */
/* ------------------------------------------------------------------ */

#[derive(Debug)]
struct SinowealthData {
    firmware_version: [u8; 2],
    firmware_version_string: String,
    is_long: bool,
    sensor: Sensor,
    led_type: LedType,
    num_buttons: usize,
    num_profiles: usize,
    config_size: usize,
    /// Per-profile raw config buffers (up to 3).
    configs: Vec<Vec<u8>>,
    /// Per-profile raw button buffers (up to 3).
    buttons: Vec<Vec<u8>>,
    active_profile: u8,
}

/* ------------------------------------------------------------------ */
/* Driver                                                               */
/* ------------------------------------------------------------------ */

pub struct SinowealthDriver {
    data: Option<SinowealthData>,
}

impl SinowealthDriver {
    pub fn new() -> Self {
        Self { data: None }
    }

    /* ---- I/O primitives ---- */

    /// Send a short command and read back the response, validating the echo.
    fn query_read(
        io: &DeviceIo,
        cmd: &[u8; SINOWEALTH_CMD_SIZE],
    ) -> Result<[u8; SINOWEALTH_CMD_SIZE]> {
        io.set_feature_report(cmd)
            .context("query_read: set_feature failed")?;
        let mut resp = [0u8; SINOWEALTH_CMD_SIZE];
        resp[0] = ReportId::Cmd as u8;
        io.get_feature_report(&mut resp)
            .context("query_read: get_feature failed")?;
        if resp[1] != cmd[1] {
            anyhow::bail!(
                "SinoWealth query_read: response command mismatch (expected {:#04x}, got {:#04x})",
                cmd[1],
                resp[1]
            );
        }
        Ok(resp)
    }

    /// Send a write-only short command.
    fn query_write(io: &DeviceIo, cmd: &[u8; SINOWEALTH_CMD_SIZE]) -> Result<()> {
        io.set_feature_report(cmd)
            .context("query_write: set_feature failed")?;
        Ok(())
    }

    /// Read a full-size report (config or button) after issuing a command.
    fn query_read_report(
        io: &DeviceIo,
        report_id: ReportId,
        cmd_id: CommandId,
        size: usize,
    ) -> Result<Vec<u8>> {
        // Step 1: issue command
        let cmd = build_cmd(cmd_id);
        io.set_feature_report(&cmd)
            .context("query_read_report: set_feature (cmd) failed")?;

        // Step 2: read the data report
        let mut buf = vec![0u8; size];
        buf[0] = report_id as u8;
        io.get_feature_report(&mut buf)
            .context("query_read_report: get_feature (data) failed")?;
        Ok(buf)
    }

    /// Write a full-size report (config or button) after issuing a command.
    fn query_write_report(io: &DeviceIo, cmd_id: CommandId, buf: &[u8]) -> Result<()> {
        let cmd = build_cmd(cmd_id);
        io.set_feature_report(&cmd)
            .context("query_write_report: set_feature (cmd) failed")?;
        io.set_feature_report(buf)
            .context("query_write_report: set_feature (data) failed")?;
        Ok(())
    }

    /* ---- is_long detection ---- */

    /// Detect whether the device uses the long config report (ID 0x06)
    /// by reading the HID report descriptor from sysfs.
    fn detect_is_long(io: &DeviceIo) -> Result<bool> {
        let hidraw_path = io.path();
        // /dev/hidraw3 → hidraw3
        let hidraw_name = hidraw_path
            .file_name()
            .and_then(|n| n.to_str())
            .context("Cannot extract hidraw name from path")?;

        let descriptor_path = format!("/sys/class/hidraw/{}/device/report_descriptor", hidraw_name);

        let descriptor = match std::fs::read(&descriptor_path) {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    "Cannot read HID descriptor at {}: {}. Assuming short config.",
                    descriptor_path, e
                );
                return Ok(false);
            }
        };

        // Parse HID descriptor items looking for Report ID (global item tag 0x85) = 0x06
        let mut i = 0;
        while i < descriptor.len() {
            let prefix = descriptor[i];
            let tag = prefix & 0xFC; // upper 6 bits
            let size = (prefix & 0x03) as usize; // lower 2 bits

            if tag == 0x84 && size >= 1 && i + 1 < descriptor.len() {
                // 0x84 = Report ID (1-byte data item: tag=0x84, size=1)
                // Actually HID spec: Report ID tag is 0x85 in short item encoding
                // prefix byte = 0b1000_01_01 = 0x85 (tag=0b100001, type=01 global, size=01)
                // Let's check properly:
            }

            // HID short item: tag is bits 7-4, type is bits 3-2, size is bits 1-0
            // Report ID: tag=1000, type=01 (global) → prefix bits [7:2]=100001, size in [1:0]
            // So prefix & 0xFC == 0x84, and size == 1 means prefix == 0x85
            if prefix == 0x85 && i + 1 < descriptor.len() {
                if descriptor[i + 1] == ReportId::ConfigLong as u8 {
                    debug!("HID descriptor contains report ID 0x06 — using long config format");
                    return Ok(true);
                }
            }

            // Advance past this item
            if size == 3 {
                // size=3 actually means 4 bytes of data in HID spec
                i += 1 + 4;
            } else {
                i += 1 + size;
            }
        }

        debug!("HID descriptor does not contain report ID 0x06 — using short config format");
        Ok(false)
    }

    /* ---- Firmware version formatting ---- */

    /// Format firmware version bytes into the string used in `.device` section names.
    /// If the first byte is an ASCII letter, format as "V{char}{hex}"; otherwise "{hex}{hex}".
    fn format_firmware_version(bytes: [u8; 2]) -> String {
        if bytes[0].is_ascii_alphabetic() {
            format!("{}{:02X}", bytes[0] as char, bytes[1])
        } else {
            format!("{:02X}{:02X}", bytes[0], bytes[1])
        }
    }

    /* ---- Config size detection ---- */

    /// Detect whether the config uses the short (123 byte) or long (167 byte) variant
    /// by inspecting trailing bytes in the config buffer.
    fn detect_config_size(config: &[u8]) -> usize {
        // The C driver checks if the data beyond offset 123 is all zeros.
        // If non-zero data exists past offset 123, it's the long format.
        let check_start = 1 + SINOWEALTH_CONFIG_SIZE_MIN; // skip report ID byte
        if config.len() > check_start {
            for &b in &config[check_start..config.len().min(1 + SINOWEALTH_CONFIG_SIZE_MAX)] {
                if b != 0 {
                    return SINOWEALTH_CONFIG_SIZE_MAX;
                }
            }
        }
        SINOWEALTH_CONFIG_SIZE_MIN
    }

    /* ---- Per-profile command IDs ---- */

    fn config_cmd(profile_idx: usize) -> Result<CommandId> {
        match profile_idx {
            0 => Ok(CommandId::GetConfig),
            1 => Ok(CommandId::GetConfig2),
            2 => Ok(CommandId::GetConfig3),
            _ => anyhow::bail!("Invalid profile index: {}", profile_idx),
        }
    }

    fn buttons_cmd(profile_idx: usize) -> Result<CommandId> {
        match profile_idx {
            0 => Ok(CommandId::GetButtons),
            1 => Ok(CommandId::GetButtons2),
            2 => Ok(CommandId::GetButtons3),
            _ => anyhow::bail!("Invalid profile index: {}", profile_idx),
        }
    }

    /* ---- Color helpers ---- */

    fn read_color(led_type: LedType, bytes: &[u8]) -> RgbColor {
        if bytes.len() < 3 {
            return RgbColor::default();
        }
        match led_type {
            LedType::Rbg => RgbColor {
                r: bytes[0],
                g: bytes[2],
                b: bytes[1],
            },
            _ => RgbColor {
                r: bytes[0],
                g: bytes[1],
                b: bytes[2],
            },
        }
    }

    fn write_color(led_type: LedType, color: RgbColor) -> [u8; 3] {
        match led_type {
            LedType::Rbg => [color.r, color.b, color.g],
            _ => [color.r, color.g, color.b],
        }
    }

    /* ---- Config parsing → DeviceInfo ---- */

    fn parse_config_into_profile(
        data: &SinowealthData,
        profile_idx: usize,
        profile: &mut ProfileInfo,
    ) {
        let cfg = &data.configs[profile_idx];
        if cfg.len() < 1 + SINOWEALTH_CONFIG_SIZE_MIN {
            warn!("Config buffer too short for profile {}", profile_idx);
            return;
        }

        // Config flags
        let config_flags = cfg[offset::CONFIG_FLAGS];
        let xy_independent = (config_flags & SINOWEALTH_XY_INDEPENDENT) != 0;

        // DPI count: high nibble = number of enabled DPI slots
        let dpi_count = (cfg[offset::DPI_COUNT] >> 4) as usize;

        // DPI slots: 8 slots × 2 bytes (raw_x, raw_y) starting at offset 7
        for i in 0..SINOWEALTH_NUM_DPIS.min(profile.resolutions.len()) {
            let base = offset::DPI_SLOTS + i * 2;
            if base + 1 >= cfg.len() {
                break;
            }
            let raw_x = cfg[base];
            let raw_y = cfg[base + 1];
            let dpi_x = data.sensor.raw_to_dpi(raw_x);
            let dpi_y = data.sensor.raw_to_dpi(raw_y);

            profile.resolutions[i].dpi = if xy_independent {
                Dpi::Separate { x: dpi_x, y: dpi_y }
            } else {
                Dpi::Unified(dpi_x)
            };
            profile.resolutions[i].is_disabled = i >= dpi_count;
        }

        // Active DPI slot
        let active_dpi = cfg[offset::DPI_ACTIVE_COLOR] as usize;
        for (i, res) in profile.resolutions.iter_mut().enumerate() {
            res.is_active = i == active_dpi;
            res.is_default = i == active_dpi;
        }

        // Report rate
        profile.report_rate = match cfg[offset::REPORT_RATE] {
            1 => 125,
            2 => 250,
            3 => 500,
            4 => 1000,
            _ => 1000,
        };
        profile.report_rates = SINOWEALTH_REPORT_RATES.to_vec();

        // Debounce: SinoWealth stores it in the command, not the config.
        // We'll read it separately if needed; for now expose the valid list.
        profile.debounces = SINOWEALTH_DEBOUNCE_TIMES.to_vec();

        // LED effect
        if data.led_type != LedType::None && !profile.leds.is_empty() {
            Self::parse_led_effect(data, cfg, &mut profile.leds[0]);
        }
    }

    fn parse_led_effect(data: &SinowealthData, cfg: &[u8], led: &mut LedInfo) {
        if cfg.len() <= offset::LED_BRIGHTNESS {
            return;
        }

        let effect = RgbEffect::from_byte(cfg[offset::LED_EFFECT]);
        let color = if cfg.len() > offset::LED_COLOR + 2 {
            Self::read_color(
                data.led_type,
                &cfg[offset::LED_COLOR..offset::LED_COLOR + 3],
            )
        } else {
            RgbColor::default()
        };

        let (mode, use_color) = match effect {
            RgbEffect::Off => (LedMode::Off, false),
            RgbEffect::Single | RgbEffect::Constant => (LedMode::Solid, true),
            RgbEffect::Breathing | RgbEffect::Breathing1 | RgbEffect::Breathing7 => {
                (LedMode::Breathing, true)
            }
            RgbEffect::Glorious | RgbEffect::Wave | RgbEffect::Tail => (LedMode::Cycle, false),
            RgbEffect::Rave | RgbEffect::Random => (LedMode::ColorWave, false),
            RgbEffect::NotSupported => (LedMode::Off, false),
        };

        led.mode = mode;
        if use_color {
            led.color = Color::from_rgb(color);
        }
        led.effect_duration = u32::from(cfg[offset::LED_SPEED]) * 100;
        led.brightness = u32::from(cfg[offset::LED_BRIGHTNESS]);
    }

    /* ---- Config encoding from DeviceInfo ---- */

    fn encode_config_from_profile(
        data: &mut SinowealthData,
        profile_idx: usize,
        profile: &ProfileInfo,
    ) {
        let cfg = &mut data.configs[profile_idx];
        if cfg.len() < 1 + SINOWEALTH_CONFIG_SIZE_MIN {
            return;
        }

        // DPI slots
        let mut dpi_count: u8 = 0;
        for (i, res) in profile.resolutions.iter().enumerate() {
            if i >= SINOWEALTH_NUM_DPIS {
                break;
            }
            let base = offset::DPI_SLOTS + i * 2;
            if base + 1 >= cfg.len() {
                break;
            }
            let (dpi_x, dpi_y) = match res.dpi {
                Dpi::Unified(d) => (d, d),
                Dpi::Separate { x, y } => (x, y),
                Dpi::Unknown => continue,
            };
            cfg[base] = data.sensor.dpi_to_raw(dpi_x).unwrap_or(0);
            cfg[base + 1] = data.sensor.dpi_to_raw(dpi_y).unwrap_or(0);
            if !res.is_disabled {
                dpi_count += 1;
            }
        }
        cfg[offset::DPI_COUNT] = (dpi_count << 4) | (cfg[offset::DPI_COUNT] & 0x0F);

        // Active DPI
        if let Some(active_idx) = profile.resolutions.iter().position(|r| r.is_active) {
            cfg[offset::DPI_ACTIVE_COLOR] = active_idx as u8;
        }

        // Report rate
        cfg[offset::REPORT_RATE] = match profile.report_rate {
            125 => 1,
            250 => 2,
            500 => 3,
            1000 => 4,
            _ => 4,
        };

        // LED effect
        if data.led_type != LedType::None {
            if let Some(led) = profile.leds.first() {
                Self::encode_led_effect(data.led_type, cfg, led);
            }
        }
    }

    fn encode_led_effect(led_type: LedType, cfg: &mut [u8], led: &LedInfo) {
        if cfg.len() <= offset::LED_BRIGHTNESS {
            return;
        }

        let (effect, use_color) = match led.mode {
            LedMode::Off => (RgbEffect::Off, false),
            LedMode::Solid => (RgbEffect::Single, true),
            LedMode::Breathing => (RgbEffect::Breathing1, true),
            LedMode::Cycle => (RgbEffect::Glorious, false),
            LedMode::ColorWave => (RgbEffect::Wave, false),
            LedMode::Starlight => (RgbEffect::Random, false),
            LedMode::TriColor => (RgbEffect::Rave, false),
        };

        cfg[offset::LED_EFFECT] = effect as u8;
        if use_color {
            let color_bytes = Self::write_color(led_type, led.color.to_rgb());
            cfg[offset::LED_COLOR..offset::LED_COLOR + 3].copy_from_slice(&color_bytes);
        }
        cfg[offset::LED_SPEED] = (led.effect_duration / 100).min(255) as u8;
        cfg[offset::LED_BRIGHTNESS] = led.brightness.min(255) as u8;
    }

    /* ---- Button parsing → DeviceInfo ---- */

    fn parse_buttons_into_profile(
        data: &SinowealthData,
        profile_idx: usize,
        profile: &mut ProfileInfo,
    ) {
        let btn_buf = &data.buttons[profile_idx];
        for (i, button) in profile.buttons.iter_mut().enumerate() {
            if i >= data.num_buttons || i >= SINOWEALTH_NUM_BUTTONS {
                break;
            }
            let off = BUTTON_ENTRY_OFFSET + i * BUTTON_ENTRY_SIZE;
            if off + BUTTON_ENTRY_SIZE > btn_buf.len() {
                break;
            }
            let raw: [u8; 4] = btn_buf[off..off + 4].try_into().unwrap_or([0; 4]);
            let (action_type, mapping_val) = decode_button(&raw);
            button.action_type = action_type;
            button.mapping_value = mapping_val;
        }
    }

    /* ---- Button encoding from DeviceInfo ---- */

    fn encode_buttons_from_profile(
        data: &mut SinowealthData,
        profile_idx: usize,
        profile: &ProfileInfo,
    ) {
        let btn_buf = &mut data.buttons[profile_idx];
        for (i, button) in profile.buttons.iter().enumerate() {
            if i >= data.num_buttons || i >= SINOWEALTH_NUM_BUTTONS {
                break;
            }
            let off = BUTTON_ENTRY_OFFSET + i * BUTTON_ENTRY_SIZE;
            if off + BUTTON_ENTRY_SIZE > btn_buf.len() {
                break;
            }
            let encoded = encode_button(button.action_type, button.mapping_value);
            btn_buf[off..off + 4].copy_from_slice(&encoded);
        }
    }

    /* ---- Macro read/write ---- */

    fn read_macro(
        io: &DeviceIo,
        report_id: ReportId,
        profile_idx: u8,
        button_idx: u8,
    ) -> Result<Vec<(u32, u32)>> {
        let mut cmd = build_cmd(CommandId::Macro);
        cmd[2] = profile_idx;
        cmd[3] = button_idx;
        io.set_feature_report(&cmd)
            .context("read_macro: set_feature (cmd)")?;

        let mut buf = vec![0u8; SINOWEALTH_MACRO_SIZE];
        buf[0] = report_id as u8;
        io.get_feature_report(&mut buf)
            .context("read_macro: get_feature")?;

        let mut events = Vec::new();
        let header = 3; // report_id + 2 bytes header
        for i in 0..SINOWEALTH_MACRO_LENGTH_MAX {
            let off = header + i * SINOWEALTH_MACRO_EVENT_SIZE;
            if off + SINOWEALTH_MACRO_EVENT_SIZE > buf.len() {
                break;
            }
            let ev_type = buf[off];
            if ev_type == 0 {
                break;
            }
            let keycode = buf[off + 1];
            // ev_type: bit 0 = press/release, keycode = HID usage
            events.push((u32::from(ev_type), u32::from(keycode)));
        }
        Ok(events)
    }

    fn write_macro(
        io: &DeviceIo,
        report_id: ReportId,
        profile_idx: u8,
        button_idx: u8,
        events: &[(u32, u32)],
    ) -> Result<()> {
        let mut buf = vec![0u8; SINOWEALTH_MACRO_SIZE];
        buf[0] = report_id as u8;
        buf[1] = profile_idx;
        buf[2] = button_idx;

        let header = 3;
        for (i, &(ev_type, keycode)) in events.iter().enumerate() {
            if i >= SINOWEALTH_MACRO_LENGTH_MAX {
                break;
            }
            let off = header + i * SINOWEALTH_MACRO_EVENT_SIZE;
            buf[off] = ev_type as u8;
            buf[off + 1] = keycode as u8;
            // buf[off + 2] = delay, left as 0 (instant)
        }

        let mut cmd = build_cmd(CommandId::Macro);
        cmd[2] = profile_idx;
        cmd[3] = button_idx;
        io.set_feature_report(&cmd)
            .context("write_macro: set_feature (cmd)")?;
        io.set_feature_report(&buf)
            .context("write_macro: set_feature (data)")?;
        Ok(())
    }
}

/* ------------------------------------------------------------------ */
/* DeviceDriver trait implementation                                     */
/* ------------------------------------------------------------------ */

#[async_trait]
impl DeviceDriver for SinowealthDriver {
    fn name(&self) -> &str {
        "SinoWealth"
    }

    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()> {
        // 1. Read firmware version
        let cmd = build_cmd(CommandId::FirmwareVersion);
        let resp = Self::query_read(io, &cmd).context("Failed to read firmware version")?;
        let fw_bytes = [resp[2], resp[3]];
        let fw_string = Self::format_firmware_version(fw_bytes);
        debug!(
            "SinoWealth firmware version: {} (raw: {:02x} {:02x})",
            fw_string, fw_bytes[0], fw_bytes[1]
        );

        // 2. Detect is_long
        let is_long = Self::detect_is_long(io)?;
        let config_report_id = if is_long {
            ReportId::ConfigLong
        } else {
            ReportId::Config
        };

        // 3. Read first config to detect config_size
        let config0 = Self::query_read_report(
            io,
            config_report_id,
            CommandId::GetConfig,
            SINOWEALTH_CONFIG_REPORT_SIZE,
        )
        .context("Failed to read initial config report")?;
        let config_size = Self::detect_config_size(&config0);
        debug!("SinoWealth config size: {} bytes", config_size);

        // 4. Store probe data (will be completed in load_profiles once we have DeviceInfo)
        self.data = Some(SinowealthData {
            firmware_version: fw_bytes,
            firmware_version_string: fw_string,
            is_long,
            sensor: Sensor::Pmw3360, // default, will be overridden in load_profiles
            led_type: LedType::None,
            num_buttons: 6,
            num_profiles: 1,
            config_size,
            configs: vec![config0],
            buttons: Vec::new(),
            active_profile: 0,
        });

        Ok(())
    }

    async fn load_profiles(&mut self, io: &mut DeviceIo, info: &mut DeviceInfo) -> Result<()> {
        let data = self
            .data
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Not probed"))?;

        // 1. Match firmware version against device file entries
        let dev_cfg = info.driver_config.sinowealth_devices.iter().find(|d| {
            d.firmware_version
                .eq_ignore_ascii_case(&data.firmware_version_string)
        });

        if let Some(cfg) = dev_cfg {
            debug!(
                "Matched firmware {} → {} (buttons={}, sensor={}, led={:?})",
                data.firmware_version_string,
                cfg.device_name,
                cfg.buttons,
                cfg.sensor_type,
                cfg.led_type
            );
            data.sensor = Sensor::from_name(&cfg.sensor_type).unwrap_or(Sensor::Pmw3360);
            data.led_type = LedType::from(cfg.led_type);
            data.num_buttons = cfg.buttons as usize;
            data.num_profiles = cfg.profiles.max(1) as usize;
        } else {
            warn!(
                "No device config for firmware version {}; using defaults (6 buttons, 1 profile, PMW3360)",
                data.firmware_version_string
            );
        }

        info.firmware_version = data.firmware_version_string.clone();

        // 2. Read remaining profile configs and all button reports
        let config_report_id = if data.is_long {
            ReportId::ConfigLong
        } else {
            ReportId::Config
        };

        for profile_idx in 1..data.num_profiles {
            let cmd_id = Self::config_cmd(profile_idx)?;
            let config = Self::query_read_report(
                io,
                config_report_id,
                cmd_id,
                SINOWEALTH_CONFIG_REPORT_SIZE,
            )
            .with_context(|| format!("Failed to read config for profile {}", profile_idx))?;
            data.configs.push(config);
        }

        for profile_idx in 0..data.num_profiles {
            let cmd_id = Self::buttons_cmd(profile_idx)?;
            let buttons = Self::query_read_report(
                io,
                config_report_id,
                cmd_id,
                SINOWEALTH_BUTTON_REPORT_SIZE,
            )
            .with_context(|| format!("Failed to read buttons for profile {}", profile_idx))?;
            data.buttons.push(buttons);
        }

        // 3. Read active profile
        let profile_cmd = build_cmd(CommandId::Profile);
        if let Ok(resp) = Self::query_read(io, &profile_cmd) {
            data.active_profile = resp[2];
            debug!("Active profile: {}", data.active_profile);
        }

        // 4. Build DPI list from sensor
        let max_dpi = data.sensor.max_dpi();
        let dpi_list: Vec<u32> = (SINOWEALTH_DPI_MIN..=max_dpi)
            .step_by(SINOWEALTH_DPI_STEP as usize)
            .collect();

        // 5. Rebuild profiles in DeviceInfo with correct counts
        let num_leds = if data.led_type != LedType::None { 1 } else { 0 };
        info.profiles = (0..data.num_profiles as u32)
            .map(|idx| ProfileInfo {
                index: idx,
                name: String::new(),
                is_active: idx == data.active_profile as u32,
                is_enabled: true,
                is_dirty: false,
                report_rate: 1000,
                report_rates: SINOWEALTH_REPORT_RATES.to_vec(),
                angle_snapping: -1,
                debounce: -1,
                debounces: SINOWEALTH_DEBOUNCE_TIMES.to_vec(),
                capabilities: Vec::new(),
                resolutions: (0..SINOWEALTH_NUM_DPIS as u32)
                    .map(|ri| crate::engine::device::ResolutionInfo {
                        index: ri,
                        dpi: Dpi::Unified(800),
                        dpi_list: dpi_list.clone(),
                        capabilities: vec![
                            crate::engine::device::RATBAG_RESOLUTION_CAP_SEPARATE_XY_RESOLUTION,
                            crate::engine::device::RATBAG_RESOLUTION_CAP_DISABLE,
                        ],
                        is_active: ri == 0,
                        is_default: ri == 0,
                        is_disabled: false,
                    })
                    .collect(),
                buttons: (0..data.num_buttons as u32)
                    .map(|bi| ButtonInfo {
                        index: bi,
                        action_type: ActionType::Button,
                        action_types: vec![0, 1, 2, 3, 4],
                        mapping_value: 0x110 + bi, // default: left, right, middle, ...
                        macro_entries: Vec::new(),
                    })
                    .collect(),
                leds: (0..num_leds as u32)
                    .map(|li| LedInfo {
                        index: li,
                        mode: LedMode::Off,
                        modes: vec![
                            LedMode::Off,
                            LedMode::Solid,
                            LedMode::Cycle,
                            LedMode::Breathing,
                            LedMode::ColorWave,
                        ],
                        color: Color::default(),
                        secondary_color: Color::default(),
                        tertiary_color: Color::default(),
                        color_depth: 1,
                        effect_duration: 0,
                        brightness: 255,
                    })
                    .collect(),
            })
            .collect();

        // 6. Parse raw buffers into the profile structs
        for profile_idx in 0..data.num_profiles {
            Self::parse_config_into_profile(data, profile_idx, &mut info.profiles[profile_idx]);
            Self::parse_buttons_into_profile(data, profile_idx, &mut info.profiles[profile_idx]);

            // Load macros for buttons that reference them
            for btn_idx in 0..data.num_buttons {
                if info.profiles[profile_idx].buttons[btn_idx].action_type == ActionType::Macro {
                    match Self::read_macro(io, config_report_id, profile_idx as u8, btn_idx as u8) {
                        Ok(events) => {
                            info.profiles[profile_idx].buttons[btn_idx].macro_entries = events;
                        }
                        Err(e) => {
                            warn!(
                                "Failed to read macro for profile {} button {}: {}",
                                profile_idx, btn_idx, e
                            );
                        }
                    }
                }
            }
        }

        debug!(
            "SinoWealth: loaded {} profile(s), {} buttons, sensor={:?}, led={:?}",
            data.num_profiles, data.num_buttons, data.sensor, data.led_type
        );

        Ok(())
    }

    async fn commit(&mut self, io: &mut DeviceIo, info: &DeviceInfo) -> Result<()> {
        let data = self
            .data
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Not probed"))?;
        let config_report_id = if data.is_long {
            ReportId::ConfigLong
        } else {
            ReportId::Config
        };

        for (profile_idx, profile) in info.profiles.iter().enumerate() {
            if !profile.is_dirty {
                continue;
            }

            // 1. Encode DPI, rate, LED into raw config buffer
            Self::encode_config_from_profile(data, profile_idx, profile);

            // 2. Write config report
            let config_cmd = Self::config_cmd(profile_idx)?;
            data.configs[profile_idx][0] = config_report_id as u8;
            Self::query_write_report(io, config_cmd, &data.configs[profile_idx])
                .with_context(|| format!("Failed to write config for profile {}", profile_idx))?;

            // 3. Encode and write button report
            Self::encode_buttons_from_profile(data, profile_idx, profile);
            let btn_cmd = Self::buttons_cmd(profile_idx)?;
            data.buttons[profile_idx][0] = config_report_id as u8;
            Self::query_write_report(io, btn_cmd, &data.buttons[profile_idx])
                .with_context(|| format!("Failed to write buttons for profile {}", profile_idx))?;

            // 4. Write macros for buttons that have them
            for (btn_idx, button) in profile.buttons.iter().enumerate() {
                if button.action_type == ActionType::Macro && !button.macro_entries.is_empty() {
                    Self::write_macro(
                        io,
                        config_report_id,
                        profile_idx as u8,
                        btn_idx as u8,
                        &button.macro_entries,
                    )
                    .with_context(|| {
                        format!(
                            "Failed to write macro for profile {} button {}",
                            profile_idx, btn_idx
                        )
                    })?;
                }
            }

            debug!("SinoWealth: committed profile {}", profile_idx);
        }

        // 5. Set debounce if specified
        if let Some(profile) = info.profiles.first() {
            if profile.debounce >= 0 {
                let mut cmd = build_cmd(CommandId::Debounce);
                cmd[2] = profile.debounce as u8;
                Self::query_write(io, &cmd).context("Failed to set debounce")?;
                debug!("SinoWealth: set debounce to {}ms", profile.debounce);
            }
        }

        // 6. Set active profile if changed
        if let Some(active) = info.profiles.iter().find(|p| p.is_active) {
            let mut cmd = build_cmd(CommandId::Profile);
            cmd[2] = active.index as u8;
            Self::query_write(io, &cmd).context("Failed to set active profile")?;
        }

        Ok(())
    }
}

/* ------------------------------------------------------------------ */
/* Helpers                                                              */
/* ------------------------------------------------------------------ */

/// Build a 6-byte command report ready to be sent as a feature report.
pub fn build_cmd(cmd_id: CommandId) -> [u8; SINOWEALTH_CMD_SIZE] {
    let mut buf = [0u8; SINOWEALTH_CMD_SIZE];
    buf[0] = ReportId::Cmd as u8;
    buf[1] = cmd_id as u8;
    buf
}
