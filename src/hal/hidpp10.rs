/* Logitech HID++ 1.0 driver implementation. */
/*  */
/* HID++ 1.0 is the older protocol used by devices like the G500, G700, G9. */
/* It uses register-based commands with short (7-byte) reports. */
/* Based on the HID++ 1.0 documentation provided by Nestor Lopez Casado. */

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::engine::device::{
    ActionType, Color, DeviceInfo, Dpi, LedMode, ProfileInfo, RgbColor,
    special_action,
};
use crate::hal::DeviceIo;

use super::hidpp::{self, HidppReport, DEVICE_IDX_CORDED, DEVICE_IDX_RECEIVER};

/* ------------------------------------------------------------------ */
/*  HID++ 1.0 register addresses                                      */
/* ------------------------------------------------------------------ */

const REG_HIDPP_NOTIFICATIONS: u8 = 0x00;
const REG_INDIVIDUAL_FEATURES: u8 = 0x01;
const REG_BATTERY_STATUS: u8 = 0x07;
const REG_BATTERY_MILEAGE: u8 = 0x0D;
const REG_CURRENT_PROFILE: u8 = 0x0F;
const REG_LED_STATUS: u8 = 0x51;
const REG_LED_INTENSITY: u8 = 0x54;
const REG_LED_COLOR: u8 = 0x57;
const REG_OPTICAL_SENSOR: u8 = 0x61;
const REG_CURRENT_RESOLUTION: u8 = 0x63;
const REG_USB_REFRESH_RATE: u8 = 0x64;
const REG_MEMORY_MANAGEMENT: u8 = 0xA0;
const REG_READ_MEMORY: u8 = 0xA2;
const REG_DEVICE_CONNECTION: u8 = 0xB2;
const REG_PAIRING_INFORMATION: u8 = 0xB5;
const REG_FIRMWARE_INFORMATION: u8 = 0xF1;

/* HID++ 1.0 sub-IDs for register access */
const SUB_ID_GET_REGISTER: u8 = 0x81;
const SUB_ID_SET_REGISTER: u8 = 0x80;
const SUB_ID_GET_LONG_REGISTER: u8 = 0x83;
const SUB_ID_SET_LONG_REGISTER: u8 = 0x82;

/* HOT (Host-Over-Transport) protocol constants */
const CMD_HOT_CONTROL: u8 = 0xA1;
const HOT_NOTIFICATION: u8 = 0x50;
const HOT_WRITE: u8 = 0x92;
const HOT_CONTINUE: u8 = 0x93;

/* Memory page constants */
const PAGE_SIZE: usize = 512;
const MAX_PAGE_NUMBER: u8 = 31;
const NUM_DPI_MODES: usize = 5;
const NUM_BUTTONS: usize = 13;
const NUM_BUTTONS_G9: usize = 10;

/* Profile type markers for the current-profile register (0x0F) */
const PROFILE_TYPE_INDEX: u8 = 0x00;
#[allow(dead_code)]
const PROFILE_TYPE_ADDRESS: u8 = 0x01;
const PROFILE_TYPE_FACTORY: u8 = 0xFF;

/* Pairing information sub-types for register 0xB5 */
#[allow(dead_code)]
const PAIRING_INFO_DEVICE: u8 = 0x20;
#[allow(dead_code)]
const PAIRING_INFO_DEVICE_NAME: u8 = 0x40;
#[allow(dead_code)]
const PAIRING_INFO_EXTENDED: u8 = 0x30;

/* Device connection/disconnection commands for register 0xB2 */
#[allow(dead_code)]
const CONNECT_OPEN_LOCK: u8 = 1;
#[allow(dead_code)]
const CONNECT_CLOSE_LOCK: u8 = 2;
#[allow(dead_code)]
const CONNECT_DISCONNECT: u8 = 3;

/* Firmware info sub-items for register 0xF1 */
#[allow(dead_code)]
const FW_INFO_NAME_AND_VERSION: u8 = 0x01;
#[allow(dead_code)]
const FW_INFO_BUILD_NUMBER: u8 = 0x02;

/* Button binding type codes from onboard profiles */
const PROFILE_BUTTON_TYPE_BUTTON: u8 = 0x81;
const PROFILE_BUTTON_TYPE_KEYS: u8 = 0x82;
const PROFILE_BUTTON_TYPE_SPECIAL: u8 = 0x83;
const PROFILE_BUTTON_TYPE_CONSUMER_CONTROL: u8 = 0x84;
const PROFILE_BUTTON_TYPE_DISABLED: u8 = 0x8F;

/* Macro event type tags. Each macro instruction starts with one of these
 * bytes; the size of the instruction varies by tag (1–5 bytes). */
const MACRO_NOOP: u8 = 0x00;
const MACRO_WAIT_FOR_BUTTON_RELEASE: u8 = 0x01;
const MACRO_REPEAT_UNTIL_BUTTON_RELEASE: u8 = 0x02;
const MACRO_REPEAT: u8 = 0x03;
const MACRO_KEY_PRESS: u8 = 0x20;
const MACRO_KEY_RELEASE: u8 = 0x21;
const MACRO_MOD_PRESS: u8 = 0x22;
const MACRO_MOD_RELEASE: u8 = 0x23;
const MACRO_MOUSE_WHEEL: u8 = 0x24;
const MACRO_MOUSE_BUTTON_PRESS: u8 = 0x40;
const MACRO_MOUSE_BUTTON_RELEASE: u8 = 0x41;
const MACRO_CONSUMER_CONTROL: u8 = 0x42;
const MACRO_DELAY: u8 = 0x43;
const MACRO_JUMP: u8 = 0x44;
const MACRO_JUMP_IF_PRESSED: u8 = 0x45;
const MACRO_MOUSE_POINTER_MOVE: u8 = 0x60;
const MACRO_JUMP_IF_RELEASED_TIMEOUT: u8 = 0x61;
const MACRO_END: u8 = 0xFF;

/* Maximum number of macro events before we bail out. */
const MAX_MACRO_EVENTS: usize = 256;

/* The receiver's own HID++ device index, used for pairing commands.
 * This happens to be the same value as DEVICE_IDX_CORDED (0xFF). */
#[allow(dead_code)]
const HIDPP_RECEIVER_IDX: u8 = 0xFF;

/* ------------------------------------------------------------------ */
/*  Enumerations                                                       */
/* ------------------------------------------------------------------ */

/* Device profile type, derived from .device configuration files. Determines
 * the binary layout of onboard profile data in flash, the DPI encoding
 * scheme (8-bit vs 16-bit), the number of buttons, and whether the device
 * supports separate X/Y resolution or RGB LEDs. */
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hidpp10ProfileType {
    #[default]
    Unknown,
    G500,
    G700,
    G9,
}

impl Hidpp10ProfileType {
    #[allow(dead_code)]
    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "G500" => Self::G500,
            "G700" => Self::G700,
            "G9" => Self::G9,
            _ => Self::Unknown,
        }
    }
}

/* Battery level as reported by register 0x07. */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
pub enum BatteryLevel {
    Unknown = 0x00,
    Critical = 0x01,
    CriticalLegacy = 0x02,
    Low = 0x03,
    LowLegacy = 0x04,
    Good = 0x05,
    GoodLegacy = 0x06,
    FullLegacy = 0x07,
}

impl BatteryLevel {
    fn from_u8(val: u8) -> Self {
        match val {
            0x01 => Self::Critical,
            0x02 => Self::CriticalLegacy,
            0x03 => Self::Low,
            0x04 => Self::LowLegacy,
            0x05 => Self::Good,
            0x06 => Self::GoodLegacy,
            0x07 => Self::FullLegacy,
            _ => Self::Unknown,
        }
    }
}

/* Battery charge state shared by registers 0x07 and 0x0D. */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
pub enum BatteryChargeState {
    NotCharging = 0x00,
    Unknown = 0x20,
    Charging = 0x21,
    ChargingComplete = 0x22,
    ChargingError = 0x23,
    ChargingFast = 0x24,
    ChargingSlow = 0x25,
    ToppingCharge = 0x26,
}

impl BatteryChargeState {
    fn from_u8(val: u8) -> Self {
        match val {
            0x21 => Self::Charging,
            0x22 => Self::ChargingComplete,
            0x23 => Self::ChargingError,
            0x24 => Self::ChargingFast,
            0x25 => Self::ChargingSlow,
            0x26 => Self::ToppingCharge,
            0x20 => Self::Unknown,
            _ if val <= 0x1F => Self::NotCharging,
            _ => Self::Unknown,
        }
    }
}

/* LED hardware status per LED, from register 0x51. Each LED occupies a
 * 4-bit nibble in the register response, supporting seven distinct modes. */
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
#[allow(dead_code)]
pub enum LedStatus {
    #[default]
    NoChange = 0x0,
    Off = 0x1,
    On = 0x2,
    Blink = 0x3,
    Heartbeat = 0x4,
    SlowOn = 0x5,
    SlowOff = 0x6,
}

impl LedStatus {
    fn from_nibble(val: u8) -> Self {
        match val & 0x0F {
            0x1 => Self::Off,
            0x2 => Self::On,
            0x3 => Self::Blink,
            0x4 => Self::Heartbeat,
            0x5 => Self::SlowOn,
            0x6 => Self::SlowOff,
            _ => Self::NoChange,
        }
    }
}

/* ------------------------------------------------------------------ */
/*  Data structures                                                    */
/* ------------------------------------------------------------------ */

/* Return type for battery status queries (register 0x07). */
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct BatteryStatusInfo {
    pub level: BatteryLevel,
    pub charge_state: BatteryChargeState,
    pub low_threshold_percent: u8,
}

/* Return type for battery mileage queries (register 0x0D). */
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct BatteryMileage {
    pub level_percent: u8,
    pub max_seconds: u32,
    pub charge_state: BatteryChargeState,
}

/* Return type for firmware queries (register 0xF1). */
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct FirmwareInfo {
    pub major: u8,
    pub minor: u8,
    pub build: u16,
}

/* Return type for pairing queries (register 0xB5). */
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PairingInfo {
    pub report_interval: u8,
    pub wpid: u16,
    pub device_type: u8,
}

/* A single entry in the device's DPI mapping table. Maps raw register
 * bytes to actual DPI values. Built from .device file data. */
#[derive(Debug, Clone, Copy)]
pub struct DpiMapping {
    pub raw_value: u8,
    pub dpi: u32,
}

/* A single DPI mode as stored in an onboard profile. */
#[derive(Debug, Clone, Copy, Default)]
pub struct Hidpp10DpiMode {
    pub xres: u32,
    pub yres: u32,
    pub leds: [bool; 4],
}

/* Button binding from an onboard profile. The interpretation depends on
 * the type byte: mouse button, keyboard key, special action, consumer
 * control, disabled, or macro reference. */
#[derive(Debug, Clone, Copy)]
pub enum Hidpp10ButtonBinding {
    Button { button: u16 },
    Keys { modifier_flags: u8, key: u8 },
    Special { code: u16 },
    ConsumerControl { usage: u16 },
    Disabled,
    Macro { page: u8, offset: u8 },
    Unknown { type_byte: u8 },
}

/* A single macro event from an onboard profile. The tag byte determines
 * which variant is active and how many bytes the event consumes in flash.
 * Variable-length instructions range from 1 byte (Noop) to 5 bytes
 * (MousePointerMove, JumpIfReleasedTimeout). Short-delay encoding (tags
 * 0x80–0xFE) is normalised into Delay events during parsing, matching
 * the four linear ranges defined in the HID++ 1.0 specification. */
#[derive(Debug, Clone, Copy)]
pub enum MacroEvent {
    Noop,
    WaitForButtonRelease,
    RepeatUntilButtonRelease,
    Repeat,
    KeyPress { key: u8 },
    KeyRelease { key: u8 },
    ModPress { key: u8 },
    ModRelease { key: u8 },
    MouseWheel { value: i8 },
    MouseButtonPress { button: u16 },
    MouseButtonRelease { button: u16 },
    ConsumerControl { usage: u16 },
    Delay { time_ms: u16 },
    Jump { page: u8, offset: u8 },
    JumpIfPressed { page: u8, offset: u8 },
    MousePointerMove { x_rel: i16, y_rel: u16 },
    JumpIfReleasedTimeout { timeout_ms: u16, page: u8, offset: u8 },
    End,
    Unknown { tag: u8 },
}

/* A fully parsed onboard profile read from flash memory. */
#[derive(Debug, Clone)]
pub struct Hidpp10Profile {
    pub page: u8,
    pub offset: u8,
    pub enabled: bool,
    pub name: String,
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub angle_correction: bool,
    pub default_dpi_mode: u8,
    pub refresh_rate: u16,
    pub dpi_modes: Vec<Hidpp10DpiMode>,
    pub buttons: Vec<Hidpp10ButtonBinding>,
    pub macros: Vec<Option<Vec<MacroEvent>>>,
}

impl Default for Hidpp10Profile {
    fn default() -> Self {
        Self {
            page: 0, offset: 0, enabled: false, name: String::new(),
            red: 0, green: 0, blue: 0, angle_correction: false,
            default_dpi_mode: 0, refresh_rate: 0,
            dpi_modes: Vec::new(), buttons: Vec::new(),
            macros: Vec::new(),
        }
    }
}

/* Short register payload types for simple 3-byte registers. */

#[derive(Debug, Clone, Copy, Default)]
pub struct Hidpp10RefreshRatePayload {
    pub rate: u8,
    pub param2: u8,
    pub param3: u8,
}

impl Hidpp10RefreshRatePayload {
    pub fn from_bytes(buf: &[u8; 3]) -> Self {
        Self { rate: buf[0], param2: buf[1], param3: buf[2] }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Hidpp10LedColorPayload {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Hidpp10LedColorPayload {
    pub fn from_bytes(buf: &[u8; 3]) -> Self {
        Self { r: buf[0], g: buf[1], b: buf[2] }
    }
}

/* Long register payload for resolution (register 0x63, long form). */
#[derive(Debug, Clone, Copy, Default)]
pub struct Hidpp10ResolutionLongPayload {
    pub xres: [u8; 2],
    pub yres: [u8; 2],
    pub padding: [u8; 12],
}

impl Hidpp10ResolutionLongPayload {
    pub fn from_bytes(buf: &[u8; 16]) -> Self {
        let mut xres = [0u8; 2];
        xres.copy_from_slice(&buf[0..2]);
        let mut yres = [0u8; 2];
        yres.copy_from_slice(&buf[2..4]);
        let mut padding = [0u8; 12];
        padding.copy_from_slice(&buf[4..16]);
        Self { xres, yres, padding }
    }
    pub fn xres(&self) -> u16 { u16::from_le_bytes(self.xres) }
    pub fn yres(&self) -> u16 { u16::from_le_bytes(self.yres) }
    #[allow(dead_code)]
    pub fn set_xres(&mut self, res: u16) { self.xres = res.to_le_bytes(); }
    #[allow(dead_code)]
    pub fn set_yres(&mut self, res: u16) { self.yres = res.to_le_bytes(); }
}

/* Protocol version stored after a successful probe. */
#[derive(Debug, Clone, Copy, Default)]
struct ProtocolVersion {
    major: u8,
    minor: u8,
}

/* ------------------------------------------------------------------ */
/*  Special action mapping table (HID++ 1.0 onboard profiles)         */
/* ------------------------------------------------------------------ */

/* Maps the 8-bit special-action code from an onboard profile button binding
 * to the ratbag special-action constant. Indices match the C reference
 * implementation's hidpp10_profiles_specials[] table. */
fn hidpp10_special_from_code(code: u8) -> u32 {
    match code {
        0x01 => special_action::WHEEL_LEFT,
        0x02 => special_action::WHEEL_RIGHT,
        0x03 => special_action::BATTERY_LEVEL,
        0x04 => special_action::RESOLUTION_UP,
        0x05 => special_action::RESOLUTION_CYCLE_UP,
        0x08 => special_action::RESOLUTION_DOWN,
        0x09 => special_action::RESOLUTION_CYCLE_DOWN,
        0x10 => special_action::PROFILE_UP,
        0x11 => special_action::PROFILE_CYCLE_UP,
        0x20 => special_action::PROFILE_DOWN,
        0x21 => special_action::PROFILE_CYCLE_DOWN,
        _ => special_action::UNKNOWN,
    }
}

#[allow(dead_code)]
fn hidpp10_code_from_special(special: u32) -> u8 {
    match special {
        x if x == special_action::WHEEL_LEFT => 0x01,
        x if x == special_action::WHEEL_RIGHT => 0x02,
        x if x == special_action::BATTERY_LEVEL => 0x03,
        x if x == special_action::RESOLUTION_UP => 0x04,
        x if x == special_action::RESOLUTION_CYCLE_UP => 0x05,
        x if x == special_action::RESOLUTION_DOWN => 0x08,
        x if x == special_action::RESOLUTION_CYCLE_DOWN => 0x09,
        x if x == special_action::PROFILE_UP => 0x10,
        x if x == special_action::PROFILE_CYCLE_UP => 0x11,
        x if x == special_action::PROFILE_DOWN => 0x20,
        x if x == special_action::PROFILE_CYCLE_DOWN => 0x21,
        _ => 0x00,
    }
}

/* ------------------------------------------------------------------ */
/*  DPI table helpers                                                  */
/* ------------------------------------------------------------------ */

/* Build a DPI mapping table from a list of DPI values (from .device file).
 * Each entry maps raw_value = (0x80 + index) to the corresponding DPI. */
#[allow(dead_code)]
fn build_dpi_table_from_list(entries: &[u32]) -> Vec<DpiMapping> {
    entries.iter().enumerate().map(|(i, &dpi)| DpiMapping {
        raw_value: (i as u8).wrapping_add(0x80),
        dpi,
    }).collect()
}

/* Build a DPI mapping table from a range specification (min, max, step).
 * Raw value 0 is reserved (DPI 0); values 1..=raw_max map linearly with
 * rounding to the nearest multiple of 25. */
#[allow(dead_code)]
fn build_dpi_table_from_range(min: u32, max: u32, step: u32) -> Vec<DpiMapping> {
    if step == 0 || max <= min {
        return Vec::new();
    }
    let raw_max = (max - min) / step;
    let mut table = Vec::with_capacity(raw_max as usize + 1);
    table.push(DpiMapping { raw_value: 0, dpi: 0 });
    for i in 1..=raw_max {
        let dpi_exact = min + step * i;
        let dpi_rounded = ((dpi_exact + 12) / 25) * 25;
        table.push(DpiMapping { raw_value: i as u8, dpi: dpi_rounded });
    }
    table
}

/* Get the maximum DPI value from the table. Returns 0 if the table is empty. */
#[allow(dead_code)]
fn dpi_table_get_max(table: &[DpiMapping]) -> u32 {
    table.iter().map(|m| m.dpi).max().unwrap_or(0)
}

/* Get the minimum non-zero DPI value from the table. Returns 0 if empty. */
#[allow(dead_code)]
fn dpi_table_get_min(table: &[DpiMapping]) -> u32 {
    table.iter().filter(|m| m.dpi > 0).map(|m| m.dpi).min().unwrap_or(0)
}

/* Look up the DPI value for a raw register byte. Falls back to raw × 50
 * when no table is loaded (the default 50× scale of the hardware). */
fn dpi_from_raw(table: &[DpiMapping], raw: u8) -> u32 {
    if table.is_empty() {
        return u32::from(raw) * 50;
    }
    table.iter()
        .find(|m| m.raw_value == raw)
        .map_or(0, |m| m.dpi)
}

/* Find the closest raw byte for a given DPI value. Falls back to val / 50
 * when no table is loaded. Uses nearest-match like the C implementation. */
fn raw_from_dpi(table: &[DpiMapping], dpi: u32) -> u8 {
    if table.is_empty() {
        return (dpi / 50).min(u32::from(u8::MAX)) as u8;
    }
    table.iter()
        .min_by_key(|m| (m.dpi as i64 - dpi as i64).unsigned_abs())
        .map_or(0, |m| m.raw_value)
}

/* ------------------------------------------------------------------ */
/*  Profile format parsing helpers                                     */
/* ------------------------------------------------------------------ */

/* Parse 3-byte button binding entries from raw profile data. */
fn parse_button_binding(data: &[u8; 3]) -> Hidpp10ButtonBinding {
    let type_byte = data[0];
    match type_byte {
        PROFILE_BUTTON_TYPE_BUTTON => {
            let flags = u16::from_le_bytes([data[1], data[2]]);
            let button = if flags == 0 { 0 } else { flags.trailing_zeros() as u16 + 1 };
            Hidpp10ButtonBinding::Button { button }
        }
        PROFILE_BUTTON_TYPE_KEYS => {
            Hidpp10ButtonBinding::Keys { modifier_flags: data[1], key: data[2] }
        }
        PROFILE_BUTTON_TYPE_SPECIAL => {
            Hidpp10ButtonBinding::Special {
                code: u16::from_le_bytes([data[1], data[2]]),
            }
        }
        PROFILE_BUTTON_TYPE_CONSUMER_CONTROL => {
            Hidpp10ButtonBinding::ConsumerControl {
                usage: u16::from_be_bytes([data[1], data[2]]),
            }
        }
        PROFILE_BUTTON_TYPE_DISABLED => Hidpp10ButtonBinding::Disabled,
        _ if type_byte & 0x80 != 0 => {
            Hidpp10ButtonBinding::Unknown { type_byte }
        }
        _ => {
            Hidpp10ButtonBinding::Macro { page: data[0], offset: data[1] }
        }
    }
}

/* Serialize a button binding back to the 3-byte on-wire format. */
fn serialize_button_binding(binding: &Hidpp10ButtonBinding) -> [u8; 3] {
    match *binding {
        Hidpp10ButtonBinding::Button { button } => {
            let flags: u16 = if button > 0 { 1u16 << (button - 1) } else { 0 };
            let le = flags.to_le_bytes();
            [PROFILE_BUTTON_TYPE_BUTTON, le[0], le[1]]
        }
        Hidpp10ButtonBinding::Keys { modifier_flags, key } => {
            [PROFILE_BUTTON_TYPE_KEYS, modifier_flags, key]
        }
        Hidpp10ButtonBinding::Special { code } => {
            let le = code.to_le_bytes();
            [PROFILE_BUTTON_TYPE_SPECIAL, le[0], le[1]]
        }
        Hidpp10ButtonBinding::ConsumerControl { usage } => {
            let be = usage.to_be_bytes();
            [PROFILE_BUTTON_TYPE_CONSUMER_CONTROL, be[0], be[1]]
        }
        Hidpp10ButtonBinding::Disabled => {
            [PROFILE_BUTTON_TYPE_DISABLED, 0x00, 0x00]
        }
        Hidpp10ButtonBinding::Macro { page, offset } => {
            [page, offset, 0x00]
        }
        Hidpp10ButtonBinding::Unknown { type_byte } => {
            [type_byte, 0x00, 0x00]
        }
    }
}

/* Parse 16-bit DPI modes (G500). Each mode: 2B BE xres, 2B BE yres, 2B LED nibbles.
 * The 16-bit values encode raw DPI codes that fit in a single byte; the high
 * byte is always zero in practice (see C implementation truncation). */
fn parse_dpi_modes_16(data: &[u8], count: usize, dpi_table: &[DpiMapping]) -> Vec<Hidpp10DpiMode> {
    let mut modes = Vec::with_capacity(count);
    for i in 0..count {
        let base = i * 6;
        if base + 6 > data.len() { break; }
        let xraw = u16::from_be_bytes([data[base], data[base + 1]]);
        let yraw = u16::from_be_bytes([data[base + 2], data[base + 3]]);
        modes.push(Hidpp10DpiMode {
            xres: dpi_from_raw(dpi_table, xraw as u8),
            yres: dpi_from_raw(dpi_table, yraw as u8),
            leds: [
                (data[base + 4] & 0x0F) == 0x02,
                (data[base + 4] >> 4) == 0x02,
                (data[base + 5] & 0x0F) == 0x02,
                (data[base + 5] >> 4) == 0x02,
            ],
        });
    }
    modes
}

/* Parse 8-bit dual DPI modes (G700). Each mode: 1B xres, 1B yres, 2B LED nibbles. */
fn parse_dpi_modes_8_dual(data: &[u8], count: usize, dpi_table: &[DpiMapping]) -> Vec<Hidpp10DpiMode> {
    let mut modes = Vec::with_capacity(count);
    for i in 0..count {
        let base = i * 4;
        if base + 4 > data.len() { break; }
        modes.push(Hidpp10DpiMode {
            xres: dpi_from_raw(dpi_table, data[base]),
            yres: dpi_from_raw(dpi_table, data[base + 1]),
            leds: [
                (data[base + 2] & 0x0F) == 0x02,
                (data[base + 2] >> 4) == 0x02,
                (data[base + 3] & 0x0F) == 0x02,
                (data[base + 3] >> 4) == 0x02,
            ],
        });
    }
    modes
}

/* Parse 8-bit single-axis DPI modes (G9). Each mode: 1B res, 2B LED nibbles. */
fn parse_dpi_modes_8(data: &[u8], count: usize, dpi_table: &[DpiMapping]) -> Vec<Hidpp10DpiMode> {
    let mut modes = Vec::with_capacity(count);
    for i in 0..count {
        let base = i * 3;
        if base + 3 > data.len() { break; }
        let dpi_val = dpi_from_raw(dpi_table, data[base]);
        modes.push(Hidpp10DpiMode {
            xres: dpi_val,
            yres: dpi_val,
            leds: [
                (data[base + 1] & 0x0F) == 0x02,
                (data[base + 1] >> 4) == 0x02,
                (data[base + 2] & 0x0F) == 0x02,
                (data[base + 2] >> 4) == 0x02,
            ],
        });
    }
    modes
}

/* Parse the LGS02 metadata block for profile names. Names are stored as
 * UCS-2 LE; we extract the low byte of each character (ASCII subset). */
fn parse_profile_name(metadata: &[u8]) -> String {
    if metadata.len() < 5 || &metadata[0..5] != b"LGS02" {
        return String::new();
    }
    let name_data = &metadata[5..];
    let mut name = String::new();
    for i in 0..23 {
        let offset = i * 2;
        if offset + 1 >= name_data.len() { break; }
        let ch = u16::from_le_bytes([name_data[offset], name_data[offset + 1]]);
        if ch == 0 { break; }
        name.push(char::from(ch as u8));
    }
    name
}

/* ------------------------------------------------------------------ */
/*  Macro parsing                                                      */
/* ------------------------------------------------------------------ */

/* Determine how many bytes a macro instruction consumes based on its tag.
 * Returns None for MACRO_END (terminates the stream) or unrecognised tags
 * outside the short-delay range. Short-delay tags (0x80–0xFE) are 1 byte. */
fn macro_instruction_size(tag: u8) -> Option<usize> {
    match tag {
        MACRO_NOOP | MACRO_WAIT_FOR_BUTTON_RELEASE
        | MACRO_REPEAT_UNTIL_BUTTON_RELEASE | MACRO_REPEAT => Some(1),

        MACRO_KEY_PRESS | MACRO_KEY_RELEASE
        | MACRO_MOD_PRESS | MACRO_MOD_RELEASE
        | MACRO_MOUSE_WHEEL => Some(2),

        MACRO_MOUSE_BUTTON_PRESS | MACRO_MOUSE_BUTTON_RELEASE
        | MACRO_CONSUMER_CONTROL | MACRO_DELAY
        | MACRO_JUMP | MACRO_JUMP_IF_PRESSED => Some(3),

        MACRO_MOUSE_POINTER_MOVE | MACRO_JUMP_IF_RELEASED_TIMEOUT => Some(5),

        MACRO_END => None,

        tag if tag >= 0x80 => Some(1),
        _ => None,
    }
}

/* Parse a single macro event from a byte slice starting at `data[pos]`.
 * Returns the parsed event and the number of bytes consumed. The slice
 * must contain at least `macro_instruction_size(tag)` bytes from `pos`. */
fn parse_macro_event(data: &[u8], pos: usize) -> Option<(MacroEvent, usize)> {
    if pos >= data.len() { return None; }
    let tag = data[pos];

    if tag == MACRO_END {
        return Some((MacroEvent::End, 1));
    }

    let size = macro_instruction_size(tag)?;
    if pos + size > data.len() { return None; }

    let event = match tag {
        MACRO_NOOP => MacroEvent::Noop,
        MACRO_WAIT_FOR_BUTTON_RELEASE => MacroEvent::WaitForButtonRelease,
        MACRO_REPEAT_UNTIL_BUTTON_RELEASE => MacroEvent::RepeatUntilButtonRelease,
        MACRO_REPEAT => MacroEvent::Repeat,

        MACRO_KEY_PRESS => MacroEvent::KeyPress { key: data[pos + 1] },
        MACRO_KEY_RELEASE => MacroEvent::KeyRelease { key: data[pos + 1] },
        MACRO_MOD_PRESS => MacroEvent::ModPress { key: data[pos + 1] },
        MACRO_MOD_RELEASE => MacroEvent::ModRelease { key: data[pos + 1] },
        MACRO_MOUSE_WHEEL => MacroEvent::MouseWheel { value: data[pos + 1] as i8 },

        MACRO_MOUSE_BUTTON_PRESS => {
            let flags = u16::from_le_bytes([data[pos + 1], data[pos + 2]]);
            MacroEvent::MouseButtonPress { button: if flags == 0 { 0 } else { flags.trailing_zeros() as u16 + 1 } }
        }
        MACRO_MOUSE_BUTTON_RELEASE => {
            let flags = u16::from_le_bytes([data[pos + 1], data[pos + 2]]);
            MacroEvent::MouseButtonRelease { button: if flags == 0 { 0 } else { flags.trailing_zeros() as u16 + 1 } }
        }
        MACRO_CONSUMER_CONTROL => {
            MacroEvent::ConsumerControl { usage: u16::from_be_bytes([data[pos + 1], data[pos + 2]]) }
        }
        MACRO_DELAY => {
            MacroEvent::Delay { time_ms: u16::from_be_bytes([data[pos + 1], data[pos + 2]]) }
        }
        MACRO_JUMP => {
            MacroEvent::Jump { page: data[pos + 1], offset: data[pos + 2] }
        }
        MACRO_JUMP_IF_PRESSED => {
            MacroEvent::JumpIfPressed { page: data[pos + 1], offset: data[pos + 2] }
        }

        MACRO_MOUSE_POINTER_MOVE => {
            MacroEvent::MousePointerMove {
                x_rel: i16::from_be_bytes([data[pos + 1], data[pos + 2]]),
                y_rel: u16::from_be_bytes([data[pos + 3], data[pos + 4]]),
            }
        }
        MACRO_JUMP_IF_RELEASED_TIMEOUT => {
            MacroEvent::JumpIfReleasedTimeout {
                timeout_ms: u16::from_be_bytes([data[pos + 1], data[pos + 2]]),
                page: data[pos + 3],
                offset: data[pos + 4],
            }
        }

        /* Short-delay encoding: four linear ranges from tags 0x80–0xFE,
         * each mapping to a progressively coarser delay granularity. */
        t if (0x80..=0x9F).contains(&t) => {
            MacroEvent::Delay { time_ms: 8 + u16::from(t - 0x80) * 4 }
        }
        t if (0xA0..=0xBF).contains(&t) => {
            MacroEvent::Delay { time_ms: 132 + u16::from(t - 0x9F) * 8 }
        }
        t if (0xC0..=0xDF).contains(&t) => {
            MacroEvent::Delay { time_ms: 388 + u16::from(t - 0xBF) * 16 }
        }
        t if (0xE0..=0xFE).contains(&t) => {
            MacroEvent::Delay { time_ms: 900 + u16::from(t - 0xDF) * 32 }
        }

        _ => MacroEvent::Unknown { tag },
    };

    Some((event, size))
}

/* ------------------------------------------------------------------ */
/*  Driver                                                             */
/* ------------------------------------------------------------------ */

pub struct Hidpp10Driver {
    device_index: u8,
    version: ProtocolVersion,
    profile_type: Hidpp10ProfileType,
    dpi_table: Vec<DpiMapping>,
    #[allow(dead_code)]
    dpi_table_is_range: bool,
    onboard_profiles: Vec<Hidpp10Profile>,
    profile_count: usize,
}

impl Hidpp10Driver {
    pub fn new() -> Self {
        Self {
            device_index: DEVICE_IDX_RECEIVER,
            version: ProtocolVersion::default(),
            profile_type: Hidpp10ProfileType::Unknown,
            dpi_table: Vec::new(),
            dpi_table_is_range: false,
            onboard_profiles: Vec::new(),
            profile_count: 1,
        }
    }

    /* ---- Register I/O primitives --------------------------------- */

    async fn try_probe_index(
        &self,
        io: &mut DeviceIo,
        idx: u8,
    ) -> Option<[u8; 3]> {
        let request = hidpp::build_short_report(
            idx, SUB_ID_GET_REGISTER, REG_HIDPP_NOTIFICATIONS,
            [0x00, 0x00, 0x00],
        );
        io.request(&request, 20, 2, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() { return None; }
            match report {
                HidppReport::Short { device_index, sub_id, address, params }
                    if device_index == idx
                        && sub_id == SUB_ID_GET_REGISTER
                        && address == REG_HIDPP_NOTIFICATIONS => Some(params),
                _ => None,
            }
        }).await.ok()
    }

    async fn short_register_request(
        &self,
        io: &mut DeviceIo,
        sub_id: u8,
        register: u8,
        params: [u8; 3],
    ) -> Result<[u8; 3]> {
        let request = hidpp::build_short_report(
            self.device_index, sub_id, register, params,
        );
        let dev_idx = self.device_index;
        io.request(&request, 20, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() { return None; }
            match report {
                HidppReport::Short { device_index, sub_id: sid, address, params }
                    if device_index == dev_idx
                        && sid == sub_id
                        && address == register => Some(params),
                _ => None,
            }
        }).await.with_context(|| format!(
            "HID++ 1.0 register 0x{register:02X} (sub_id=0x{sub_id:02X}) failed"
        ))
    }

    async fn get_register(&self, io: &mut DeviceIo, register: u8, params: [u8; 3]) -> Result<[u8; 3]> {
        self.short_register_request(io, SUB_ID_GET_REGISTER, register, params).await
    }

    async fn set_register(&self, io: &mut DeviceIo, register: u8, params: [u8; 3]) -> Result<[u8; 3]> {
        self.short_register_request(io, SUB_ID_SET_REGISTER, register, params).await
    }

    async fn long_register_request(
        &self,
        io: &mut DeviceIo,
        sub_id: u8,
        register: u8,
        payload: [u8; 16],
    ) -> Result<[u8; 16]> {
        let request = hidpp::build_long_report(
            self.device_index, sub_id, register, payload,
        );
        let dev_idx = self.device_index;
        io.request(&request, 20, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() { return None; }
            match report {
                HidppReport::Long { device_index, sub_id: sid, address, params }
                    if device_index == dev_idx
                        && sid == sub_id
                        && address == register => Some(params),
                _ => None,
            }
        }).await.with_context(|| format!(
            "HID++ 1.0 long register 0x{register:02X} (sub_id=0x{sub_id:02X}) failed"
        ))
    }

    async fn get_long_register(&self, io: &mut DeviceIo, register: u8) -> Result<[u8; 16]> {
        self.long_register_request(io, SUB_ID_GET_LONG_REGISTER, register, [0; 16]).await
    }

    async fn set_long_register(&self, io: &mut DeviceIo, register: u8, payload: [u8; 16]) -> Result<[u8; 16]> {
        self.long_register_request(io, SUB_ID_SET_LONG_REGISTER, register, payload).await
    }

    /* ---- HOT (Host-Over-Transport) payload system ----------------- */

    async fn hot_ctrl_reset(&self, io: &mut DeviceIo) -> Result<()> {
        let request = hidpp::build_short_report(
            self.device_index, SUB_ID_SET_REGISTER, CMD_HOT_CONTROL,
            [0x01, 0x00, 0x00],
        );
        let dev_idx = self.device_index;
        io.request(&request, 20, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() { return None; }
            match report {
                HidppReport::Short { device_index, sub_id, address, .. }
                    if device_index == dev_idx
                        && sub_id == SUB_ID_SET_REGISTER
                        && address == CMD_HOT_CONTROL => Some(()),
                _ => None,
            }
        }).await.context("HID++ 1.0 HOT ctrl reset failed")
    }

    async fn hot_request_command(&self, io: &mut DeviceIo, data: [u8; 20], expected_id: u8) -> Result<()> {
        let dev_idx = self.device_index;
        io.request(&data, 20, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() { return None; }
            match report {
                HidppReport::Long { device_index, sub_id, params, .. }
                    if device_index == dev_idx
                        && sub_id == HOT_NOTIFICATION
                        && params[0] == expected_id => Some(()),
                _ => None,
            }
        }).await.context("HID++ 1.0 HOT request command failed")
    }

    async fn send_hot_chunk(
        &self,
        io: &mut DeviceIo,
        index: u8,
        first: bool,
        dst_page: u8,
        dst_offset: u16,
        data: &[u8],
    ) -> Result<usize> {
        let mut buffer = [0u8; 20];
        buffer[0] = hidpp::REPORT_ID_LONG;
        buffer[1] = self.device_index;
        let mut offset = 2;

        if first {
            if !dst_offset.is_multiple_of(2) {
                return Err(anyhow::anyhow!("Writing memory with odd offset is not supported"));
            }
            buffer[offset] = HOT_WRITE; offset += 1;
            buffer[offset] = index; offset += 1;
            let mut header = [0u8; 9];
            header[0] = 0x01;
            header[1] = dst_page;
            header[2] = (dst_offset / 2) as u8;
            header[5..7].copy_from_slice(&(data.len() as u16).to_be_bytes());
            buffer[offset..offset + 9].copy_from_slice(&header);
            offset += 9;
        } else {
            buffer[offset] = HOT_CONTINUE; offset += 1;
            buffer[offset] = index; offset += 1;
        }

        let count = data.len().min(20 - offset);
        if count == 0 {
            return Err(anyhow::anyhow!("Invalid chunk size"));
        }
        buffer[offset..offset + count].copy_from_slice(&data[..count]);
        self.hot_request_command(io, buffer, index).await?;
        Ok(count)
    }

    async fn send_hot_payload(
        &self,
        io: &mut DeviceIo,
        dst_page: u8,
        dst_offset: u16,
        data: &[u8],
    ) -> Result<()> {
        self.hot_ctrl_reset(io).await?;
        let mut first = true;
        let mut count = 0;
        let mut index: u8 = 0;
        while count < data.len() {
            /* On first=true, pass the full data slice so send_hot_chunk can
             * encode data.len() into the HOT header as the total size. */
            let chunk_data = if first { data } else { &data[count..] };
            let written = self.send_hot_chunk(
                io, index, first, dst_page, dst_offset, chunk_data,
            ).await?;
            first = false;
            count += written;
            index = index.wrapping_add(1);
        }
        Ok(())
    }

    /* ---- Memory system (registers 0xA0 and 0xA2) ------------------ */

    /* Read 16 bytes from device memory at (page, offset). The offset must
     * be even; the hardware addresses in word (2-byte) units internally. */
    async fn read_memory(&self, io: &mut DeviceIo, page: u8, offset: u16) -> Result<[u8; 16]> {
        if !offset.is_multiple_of(2) {
            return Err(anyhow::anyhow!("Reading memory with odd offset is not supported"));
        }
        if page > MAX_PAGE_NUMBER {
            return Err(anyhow::anyhow!("Page number {page} exceeds maximum {MAX_PAGE_NUMBER}"));
        }
        let word_offset = (offset / 2) as u8;
        let request = hidpp::build_short_report(
            self.device_index, SUB_ID_GET_LONG_REGISTER, REG_READ_MEMORY,
            [page, word_offset, 0x00],
        );
        let dev_idx = self.device_index;
        io.request(&request, 20, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() { return None; }
            match report {
                HidppReport::Long { device_index, sub_id, address, params }
                    if device_index == dev_idx
                        && sub_id == SUB_ID_GET_LONG_REGISTER
                        && address == REG_READ_MEMORY => Some(params),
                _ => None,
            }
        }).await.with_context(|| format!(
            "HID++ 1.0 read_memory(page={page}, offset={offset}) failed"
        ))
    }

    /* Read a full 512-byte page and validate the CRC-CCITT. */
    async fn read_page(&self, io: &mut DeviceIo, page: u8) -> Result<[u8; PAGE_SIZE]> {
        let mut data = [0u8; PAGE_SIZE];
        for i in (0..PAGE_SIZE).step_by(16) {
            let chunk = self.read_memory(io, page, i as u16).await?;
            data[i..i + 16].copy_from_slice(&chunk);
        }
        let computed_crc = hidpp::compute_ccitt_crc(&data[..PAGE_SIZE - 2]);
        let stored_crc = u16::from_be_bytes([data[PAGE_SIZE - 2], data[PAGE_SIZE - 1]]);
        if computed_crc != stored_crc {
            return Err(anyhow::anyhow!(
                "CRC mismatch on page {page}: computed 0x{computed_crc:04X}, \
                 stored 0x{stored_crc:04X}"
            ));
        }
        Ok(data)
    }

    /* Erase a flash page via register 0xA0. */
    async fn erase_memory(&self, io: &mut DeviceIo, page: u8) -> Result<()> {
        debug!("HID++ 1.0: erasing flash page 0x{page:02X}");
        let mut payload = [0u8; 16];
        payload[0] = 0x02;
        payload[6] = page;
        self.set_long_register(io, REG_MEMORY_MANAGEMENT, payload).await?;
        Ok(())
    }

    /* Copy data between locations in device memory via register 0xA0. */
    async fn write_flash(
        &self, io: &mut DeviceIo,
        src_page: u8, src_offset: u16,
        dst_page: u8, dst_offset: u16,
        size: u16,
    ) -> Result<()> {
        if !src_offset.is_multiple_of(2) || !dst_offset.is_multiple_of(2) {
            return Err(anyhow::anyhow!("Accessing memory with odd offset is not supported"));
        }
        debug!(
            "HID++ 1.0: copying {size} bytes ({src_page:02X},{src_offset:04X}) \
             -> ({dst_page:02X},{dst_offset:04X})"
        );
        let mut payload = [0u8; 16];
        payload[0] = 0x03;
        payload[2] = src_page;
        payload[3] = (src_offset / 2) as u8;
        payload[6] = dst_page;
        payload[7] = (dst_offset / 2) as u8;
        payload[10] = (size >> 8) as u8;
        payload[11] = (size & 0xFF) as u8;
        self.set_long_register(io, REG_MEMORY_MANAGEMENT, payload).await?;
        Ok(())
    }

    /* ---- Macro reading from flash ---------------------------------- */

    /* Read a complete macro starting at (page, byte_offset) in flash.
     * Macros span across 16-byte memory reads and may contain JUMP
     * instructions that redirect to a different page/offset, which this
     * method follows transparently (matching the C implementation). The
     * method accumulates events into a Vec until it hits MACRO_END or
     * exceeds MAX_MACRO_EVENTS, preventing runaway reads. */
    async fn read_macro(
        &self, io: &mut DeviceIo, start_page: u8, start_byte_offset: u8,
    ) -> Result<Vec<MacroEvent>> {
        let mut events = Vec::new();
        let mut page = start_page;
        let mut byte_offset = start_byte_offset;
        /* Pre-fetch the 16-byte chunk that contains `byte_offset`. */
        let mut chunk = self.read_memory(io, page, u16::from(byte_offset & 0xFE)).await?;
        let mut chunk_base = u16::from(byte_offset & 0xFE);

        /* Flatten the chunk into a working buffer that can span two
         * consecutive 16-byte reads when an instruction straddles a
         * boundary. We keep a sliding window of up to 32 bytes. */
        let mut buf = [0u8; 32];
        buf[..16].copy_from_slice(&chunk);
        let mut buf_len: usize = 16;
        let mut buf_offset = usize::from(byte_offset) - chunk_base as usize;

        loop {
            if events.len() >= MAX_MACRO_EVENTS {
                warn!("HID++ 1.0: macro exceeded {MAX_MACRO_EVENTS} events, truncating");
                break;
            }

            /* Ensure we have enough data ahead for the largest possible
             * instruction (5 bytes). If not, fetch the next chunk. */
            if buf_offset + 5 > buf_len && buf_len < 32 {
                let next_addr = chunk_base + 16;
                if next_addr <= 0xFF {
                    let next_chunk = self.read_memory(io, page, next_addr).await?;
                    buf[16..32].copy_from_slice(&next_chunk);
                    buf_len = 32;
                }
            }

            if buf_offset >= buf_len { break; }

            let Some((event, size)) = parse_macro_event(&buf[..buf_len], buf_offset) else {
                break;
            };

            match event {
                MacroEvent::End => {
                    events.push(MacroEvent::End);
                    break;
                }
                MacroEvent::Jump { page: jp, offset: jo } => {
                    /* Follow the jump transparently — do not store it. */
                    page = jp;
                    byte_offset = jo.wrapping_mul(2);
                    chunk_base = u16::from(byte_offset & 0xFE);
                    chunk = self.read_memory(io, page, chunk_base).await?;
                    buf[..16].copy_from_slice(&chunk);
                    buf_len = 16;
                    buf_offset = usize::from(byte_offset) - chunk_base as usize;
                    continue;
                }
                _ => {
                    events.push(event);
                }
            }

            buf_offset += size;

            /* If we've consumed past the first chunk into the second, shift. */
            if buf_offset >= 16 && buf_len == 32 {
                buf.copy_within(16..32, 0);
                buf_len = 16;
                buf_offset -= 16;
                chunk_base += 16;
            }
            /* If we've exhausted the current buffer entirely, fetch next. */
            if buf_offset >= buf_len {
                let next_addr = chunk_base + buf_len as u16;
                if next_addr > 0xFF { break; }
                chunk = self.read_memory(io, page, next_addr).await?;
                buf[..16].copy_from_slice(&chunk);
                buf_len = 16;
                buf_offset = 0;
                chunk_base = next_addr;
            }
        }
        Ok(events)
    }

    /* ---- Register 0x00: HID++ Notifications ----------------------- */

    #[allow(dead_code)]
    async fn get_hidpp_notifications(&self, io: &mut DeviceIo) -> Result<u32> {
        let p = self.get_register(io, REG_HIDPP_NOTIFICATIONS, [0, 0, 0]).await?;
        Ok(u32::from(p[0]) | (u32::from(p[1] & 0x1F) << 8) | (u32::from(p[2] & 0x07) << 16))
    }

    #[allow(dead_code)]
    async fn set_hidpp_notifications(&self, io: &mut DeviceIo, flags: u32) -> Result<()> {
        self.set_register(io, REG_HIDPP_NOTIFICATIONS, [
            (flags & 0xFF) as u8,
            ((flags >> 8) & 0x1F) as u8,
            ((flags >> 16) & 0x07) as u8,
        ]).await?;
        Ok(())
    }

    /* ---- Register 0x01: Individual Features ----------------------- */

    #[allow(dead_code)]
    async fn get_individual_features(&self, io: &mut DeviceIo) -> Result<u32> {
        let p = self.get_register(io, REG_INDIVIDUAL_FEATURES, [0, 0, 0]).await?;
        Ok(u32::from(p[0]) | (u32::from(p[1] & 0x0E) << 8) | (u32::from(p[2] & 0x3F) << 16))
    }

    #[allow(dead_code)]
    async fn set_individual_features(&self, io: &mut DeviceIo, mask: u32) -> Result<()> {
        self.set_register(io, REG_INDIVIDUAL_FEATURES, [
            (mask & 0xFF) as u8,
            ((mask >> 8) & 0x0E) as u8,
            ((mask >> 16) & 0x3F) as u8,
        ]).await?;
        Ok(())
    }

    /* ---- Register 0x07: Battery Status ---------------------------- */

    #[allow(dead_code)]
    async fn get_battery_status(&self, io: &mut DeviceIo) -> Result<BatteryStatusInfo> {
        let p = self.get_register(io, REG_BATTERY_STATUS, [0, 0, 0]).await?;
        let mut threshold = p[2];
        if threshold >= 7 { threshold = 0; }
        threshold *= 5;
        Ok(BatteryStatusInfo {
            level: BatteryLevel::from_u8(p[0]),
            charge_state: BatteryChargeState::from_u8(p[1]),
            low_threshold_percent: threshold,
        })
    }

    /* ---- Register 0x0D: Battery Mileage --------------------------- */

    #[allow(dead_code)]
    async fn get_battery_mileage(&self, io: &mut DeviceIo) -> Result<BatteryMileage> {
        let p = self.get_register(io, REG_BATTERY_MILEAGE, [0, 0, 0]).await?;
        let mut max = u32::from(p[1]) | (u32::from(p[2] & 0x0F) << 8);
        match (p[2] & 0x30) >> 4 {
            0x03 => max *= 24 * 60 * 60,
            0x02 => max *= 60 * 60,
            0x01 => max *= 60,
            _ => {}
        }
        let charge_state = match p[2] >> 6 {
            0x01 => BatteryChargeState::Charging,
            0x02 => BatteryChargeState::ChargingComplete,
            0x03 => BatteryChargeState::ChargingError,
            _ => BatteryChargeState::NotCharging,
        };
        Ok(BatteryMileage {
            level_percent: p[0] & 0x7F,
            max_seconds: max,
            charge_state,
        })
    }

    /* ---- Register 0x51: LED Status -------------------------------- */

    #[allow(dead_code)]
    async fn get_led_status(&self, io: &mut DeviceIo) -> Result<[LedStatus; 6]> {
        let p = self.get_register(io, REG_LED_STATUS, [0, 0, 0]).await?;
        Ok([
            LedStatus::from_nibble(p[0]),
            LedStatus::from_nibble(p[0] >> 4),
            LedStatus::from_nibble(p[1]),
            LedStatus::from_nibble(p[1] >> 4),
            LedStatus::from_nibble(p[2]),
            LedStatus::from_nibble(p[2] >> 4),
        ])
    }

    #[allow(dead_code)]
    async fn set_led_status(&self, io: &mut DeviceIo, leds: &[LedStatus; 6]) -> Result<()> {
        self.set_register(io, REG_LED_STATUS, [
            (leds[0] as u8) | ((leds[1] as u8) << 4),
            (leds[2] as u8) | ((leds[3] as u8) << 4),
            (leds[4] as u8) | ((leds[5] as u8) << 4),
        ]).await?;
        Ok(())
    }

    /* ---- Register 0x54: LED Intensity ----------------------------- */

    #[allow(dead_code)]
    async fn get_led_intensity(&self, io: &mut DeviceIo) -> Result<[u8; 6]> {
        let p = self.get_register(io, REG_LED_INTENSITY, [0, 0, 0]).await?;
        Ok([
            10 * (p[0] & 0x0F),       10 * ((p[0] >> 4) & 0x0F),
            10 * (p[1] & 0x0F),       10 * ((p[1] >> 4) & 0x0F),
            10 * (p[2] & 0x0F),       10 * ((p[2] >> 4) & 0x0F),
        ])
    }

    #[allow(dead_code)]
    async fn set_led_intensity(&self, io: &mut DeviceIo, pcts: &[u8; 6]) -> Result<()> {
        self.set_register(io, REG_LED_INTENSITY, [
            (pcts[0] / 10) | ((pcts[1] / 10) << 4),
            (pcts[2] / 10) | ((pcts[3] / 10) << 4),
            (pcts[4] / 10) | ((pcts[5] / 10) << 4),
        ]).await?;
        Ok(())
    }

    /* ---- Register 0x61: Optical Sensor Settings ------------------- */

    #[allow(dead_code)]
    async fn get_optical_sensor_settings(&self, io: &mut DeviceIo) -> Result<u8> {
        let p = self.get_register(io, REG_OPTICAL_SENSOR, [0, 0, 0]).await?;
        Ok(p[0])
    }

    /* ---- Register 0xB2: Device Connection / Disconnection --------- */

    #[allow(dead_code)]
    async fn open_pairing_lock(&self, io: &mut DeviceIo, timeout: u8) -> Result<()> {
        let request = hidpp::build_short_report(
            HIDPP_RECEIVER_IDX, SUB_ID_SET_REGISTER, REG_DEVICE_CONNECTION,
            [CONNECT_OPEN_LOCK, 0xFF, timeout],
        );
        io.request(&request, 20, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() { return None; }
            match report {
                HidppReport::Short { device_index, sub_id, address, .. }
                    if device_index == HIDPP_RECEIVER_IDX
                        && sub_id == SUB_ID_SET_REGISTER
                        && address == REG_DEVICE_CONNECTION => Some(()),
                _ => None,
            }
        }).await.context("HID++ 1.0 open pairing lock failed")
    }

    #[allow(dead_code)]
    async fn close_pairing_lock(&self, io: &mut DeviceIo) -> Result<()> {
        let request = hidpp::build_short_report(
            HIDPP_RECEIVER_IDX, SUB_ID_SET_REGISTER, REG_DEVICE_CONNECTION,
            [CONNECT_CLOSE_LOCK, 0xFF, 0x00],
        );
        io.request(&request, 20, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() { return None; }
            match report {
                HidppReport::Short { device_index, sub_id, address, .. }
                    if device_index == HIDPP_RECEIVER_IDX
                        && sub_id == SUB_ID_SET_REGISTER
                        && address == REG_DEVICE_CONNECTION => Some(()),
                _ => None,
            }
        }).await.context("HID++ 1.0 close pairing lock failed")
    }

    #[allow(dead_code)]
    async fn disconnect_device(&self, io: &mut DeviceIo, device_idx: u8) -> Result<()> {
        let request = hidpp::build_short_report(
            HIDPP_RECEIVER_IDX, SUB_ID_SET_REGISTER, REG_DEVICE_CONNECTION,
            [CONNECT_DISCONNECT, device_idx, 0x00],
        );
        io.request(&request, 20, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() { return None; }
            match report {
                HidppReport::Short { device_index, sub_id, address, .. }
                    if device_index == HIDPP_RECEIVER_IDX
                        && sub_id == SUB_ID_SET_REGISTER
                        && address == REG_DEVICE_CONNECTION => Some(()),
                _ => None,
            }
        }).await.context("HID++ 1.0 disconnect device failed")
    }

    /* ---- Register 0xB5: Pairing Information ----------------------- */

    #[allow(dead_code)]
    async fn get_pairing_information(&self, io: &mut DeviceIo) -> Result<PairingInfo> {
        let request = hidpp::build_short_report(
            HIDPP_RECEIVER_IDX, SUB_ID_GET_LONG_REGISTER, REG_PAIRING_INFORMATION,
            [PAIRING_INFO_DEVICE + self.device_index - 1, 0x00, 0x00],
        );
        let resp = io.request(&request, 20, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() { return None; }
            match report {
                HidppReport::Long { device_index, sub_id, address, params }
                    if device_index == HIDPP_RECEIVER_IDX
                        && sub_id == SUB_ID_GET_LONG_REGISTER
                        && address == REG_PAIRING_INFORMATION => Some(params),
                _ => None,
            }
        }).await.context("HID++ 1.0 get pairing info failed")?;
        Ok(PairingInfo {
            report_interval: resp[2],
            wpid: u16::from_be_bytes([resp[3], resp[4]]),
            device_type: resp[7],
        })
    }

    #[allow(dead_code)]
    async fn get_pairing_device_name(&self, io: &mut DeviceIo) -> Result<String> {
        let request = hidpp::build_short_report(
            HIDPP_RECEIVER_IDX, SUB_ID_GET_LONG_REGISTER, REG_PAIRING_INFORMATION,
            [PAIRING_INFO_DEVICE_NAME + self.device_index - 1, 0x00, 0x00],
        );
        let resp = io.request(&request, 20, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() { return None; }
            match report {
                HidppReport::Long { device_index, sub_id, address, params }
                    if device_index == HIDPP_RECEIVER_IDX
                        && sub_id == SUB_ID_GET_LONG_REGISTER
                        && address == REG_PAIRING_INFORMATION => Some(params),
                _ => None,
            }
        }).await.context("HID++ 1.0 get pairing device name failed")?;
        let name_len = resp[1] as usize;
        let end = (2 + name_len).min(resp.len());
        Ok(String::from_utf8_lossy(&resp[2..end]).into_owned())
    }

    #[allow(dead_code)]
    async fn get_extended_pairing_info(&self, io: &mut DeviceIo) -> Result<u32> {
        let request = hidpp::build_short_report(
            HIDPP_RECEIVER_IDX, SUB_ID_GET_LONG_REGISTER, REG_PAIRING_INFORMATION,
            [PAIRING_INFO_EXTENDED + self.device_index - 1, 0x00, 0x00],
        );
        let resp = io.request(&request, 20, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() { return None; }
            match report {
                HidppReport::Long { device_index, sub_id, address, params }
                    if device_index == HIDPP_RECEIVER_IDX
                        && sub_id == SUB_ID_GET_LONG_REGISTER
                        && address == REG_PAIRING_INFORMATION => Some(params),
                _ => None,
            }
        }).await.context("HID++ 1.0 get extended pairing info failed")?;
        Ok(u32::from_be_bytes([resp[1], resp[2], resp[3], resp[4]]))
    }

    /* ---- Register 0xF1: Firmware Information ----------------------- */

    #[allow(dead_code)]
    async fn get_firmware_information(&self, io: &mut DeviceIo) -> Result<FirmwareInfo> {
        let ver = self.get_register(
            io, REG_FIRMWARE_INFORMATION, [FW_INFO_NAME_AND_VERSION, 0, 0],
        ).await?;
        let bld = self.get_register(
            io, REG_FIRMWARE_INFORMATION, [FW_INFO_BUILD_NUMBER, 0, 0],
        ).await?;
        Ok(FirmwareInfo {
            major: ver[1],
            minor: ver[2],
            build: u16::from_be_bytes([bld[1], bld[2]]),
        })
    }

    /* ---- Resolution (register 0x63) ------------------------------- */

    async fn read_resolution(&self, io: &mut DeviceIo, profile: &mut ProfileInfo) -> Result<()> {
        match self.profile_type {
            Hidpp10ProfileType::G9 => {
                /* G9 uses the short register for current resolution. */
                let params = self.get_register(io, REG_CURRENT_RESOLUTION, [0, 0, 0]).await?;
                let raw = u16::from_le_bytes([params[0], params[1]]);
                let dpi_val = dpi_from_raw(&self.dpi_table, raw as u8);
                if let Some(res) = profile.resolutions.first_mut() {
                    res.dpi = Dpi::Unified(dpi_val);
                }
            }
            _ => {
                /* All other devices use the long register with separate X/Y. */
                let payload = self.get_long_register(io, REG_CURRENT_RESOLUTION).await?;
                let rp = Hidpp10ResolutionLongPayload::from_bytes(&payload);
                let x_dpi = dpi_from_raw(&self.dpi_table, rp.xres() as u8);
                let y_dpi = dpi_from_raw(&self.dpi_table, rp.yres() as u8);
                if let Some(res) = profile.resolutions.first_mut() {
                    if x_dpi == y_dpi {
                        res.dpi = Dpi::Unified(x_dpi);
                    } else {
                        res.dpi = Dpi::Separate { x: x_dpi, y: y_dpi };
                    }
                }
            }
        }
        Ok(())
    }

    async fn write_resolution(&self, io: &mut DeviceIo, profile: &ProfileInfo) -> Result<()> {
        let Some(res) = profile.resolutions.iter().find(|r| r.is_active) else {
            return Ok(());
        };
        let (x_dpi, y_dpi) = match res.dpi {
            Dpi::Unified(val) => (val, val),
            Dpi::Separate { x, y } => (x, y),
            Dpi::Unknown => return Ok(()),
        };
        match self.profile_type {
            Hidpp10ProfileType::G9 => {
                let raw = raw_from_dpi(&self.dpi_table, x_dpi);
                self.set_register(io, REG_CURRENT_RESOLUTION, [raw, 0, 0]).await?;
            }
            _ => {
                let x_raw = raw_from_dpi(&self.dpi_table, x_dpi);
                let y_raw = raw_from_dpi(&self.dpi_table, y_dpi);
                let mut bytes = [0u8; 16];
                bytes[0..2].copy_from_slice(&u16::from(x_raw).to_le_bytes());
                bytes[2..4].copy_from_slice(&u16::from(y_raw).to_le_bytes());
                self.set_long_register(io, REG_CURRENT_RESOLUTION, bytes).await?;
            }
        }
        debug!("HID++ 1.0: committed DPI = {x_dpi}×{y_dpi}");
        Ok(())
    }

    /* ---- Refresh rate (register 0x64) ----------------------------- */

    async fn read_refresh_rate(&self, io: &mut DeviceIo, profile: &mut ProfileInfo) -> Result<()> {
        let params = self.get_register(io, REG_USB_REFRESH_RATE, [0, 0, 0]).await?;
        let payload = Hidpp10RefreshRatePayload::from_bytes(&params);
        if payload.rate > 0 {
            profile.report_rate = 1000 / u32::from(payload.rate);
        }
        Ok(())
    }

    async fn write_refresh_rate(&self, io: &mut DeviceIo, profile: &ProfileInfo) -> Result<()> {
        if profile.report_rate > 0 {
            let rate = (1000 / profile.report_rate).min(u32::from(u8::MAX)) as u8;
            self.set_register(io, REG_USB_REFRESH_RATE, [rate, 0, 0]).await?;
            debug!("HID++ 1.0: committed report rate = {} Hz", profile.report_rate);
        }
        Ok(())
    }

    /* ---- LED color (register 0x57) -------------------------------- */

    async fn read_led_color(&self, io: &mut DeviceIo, profile: &mut ProfileInfo) -> Result<()> {
        let cp = self.get_register(io, REG_LED_COLOR, [0, 0, 0]).await?;
        let c = Hidpp10LedColorPayload::from_bytes(&cp);
        for led in &mut profile.leds {
            led.color = Color::from_rgb(RgbColor { r: c.r, g: c.g, b: c.b });
            led.mode = LedMode::Solid;
        }
        Ok(())
    }

    async fn write_led_color(&self, io: &mut DeviceIo, profile: &ProfileInfo) -> Result<()> {
        if let Some(first_led) = profile.leds.first() {
            let rgb = first_led.color.to_rgb();
            self.set_register(io, REG_LED_COLOR, [rgb.r, rgb.g, rgb.b]).await?;
            debug!("HID++ 1.0: committed LED color");
        }
        Ok(())
    }

    /* ---- Current profile (register 0x0F) with full type handling -- */

    async fn read_current_profile(&self, io: &mut DeviceIo) -> Result<u32> {
        let params = self.get_register(io, REG_CURRENT_PROFILE, [0, 0, 0]).await?;
        let ptype = params[0];
        let page = params[1];
        let offset = params[2];

        match ptype {
            PROFILE_TYPE_INDEX => {
                let idx = u32::from(page);
                if idx as usize > self.profile_count { return Ok(0); }
                Ok(idx)
            }
            PROFILE_TYPE_ADDRESS => {
                for (i, p) in self.onboard_profiles.iter().enumerate() {
                    if p.page == page && p.offset == offset {
                        return Ok(i as u32);
                    }
                }
                warn!("HID++ 1.0: profile address ({page},{offset}) not in directory");
                Ok(0)
            }
            PROFILE_TYPE_FACTORY => {
                info!("HID++ 1.0: factory profile active, switching to profile 0");
                if let Err(e) = self.set_register(
                    io, REG_CURRENT_PROFILE, [PROFILE_TYPE_INDEX, 0x00, 0x00],
                ).await {
                    warn!("HID++ 1.0: failed to switch from factory profile: {e}");
                }
                Ok(0)
            }
            _ => {
                warn!("HID++ 1.0: unexpected profile type: 0x{ptype:02X}");
                Ok(0)
            }
        }
    }

    /* ---- Profile directory (page 1 of flash) ---------------------- */

    async fn read_profile_directory(&mut self, io: &mut DeviceIo) -> Result<()> {
        if self.profile_type == Hidpp10ProfileType::Unknown {
            return Ok(());
        }
        let page_data = match self.read_page(io, 0x01).await {
            Ok(data) => data,
            Err(e) => {
                warn!("HID++ 1.0: failed to read profile directory: {e}");
                return Ok(());
            }
        };

        self.onboard_profiles.clear();
        for i in 0..self.profile_count {
            let base = i * 3;
            if base + 2 >= page_data.len() { break; }
            let page = page_data[base];
            if page == 0xFF { break; }
            let mut profile = Hidpp10Profile::default();
            profile.page = page;
            profile.offset = page_data[base + 1];
            profile.enabled = true;
            self.onboard_profiles.push(profile);
        }
        while self.onboard_profiles.len() < self.profile_count {
            self.onboard_profiles.push(Hidpp10Profile::default());
        }
        debug!(
            "HID++ 1.0: directory: {} enabled profiles",
            self.onboard_profiles.iter().filter(|p| p.enabled).count()
        );
        Ok(())
    }

    #[allow(dead_code)]
    async fn write_profile_directory(&self, io: &mut DeviceIo) -> Result<()> {
        if self.profile_type == Hidpp10ProfileType::Unknown { return Ok(()); }
        let mut bytes = [0xFFu8; PAGE_SIZE];
        let mut index = 0usize;
        for p in &self.onboard_profiles {
            if !p.enabled { continue; }
            let base = index * 3;
            bytes[base] = p.page;
            bytes[base + 1] = p.offset;
            bytes[base + 2] = ((0b111u8 << index) >> 2) & 0b111;
            index += 1;
        }
        let crc = hidpp::compute_ccitt_crc(&bytes[..PAGE_SIZE - 2]);
        bytes[PAGE_SIZE - 2] = (crc >> 8) as u8;
        bytes[PAGE_SIZE - 1] = (crc & 0xFF) as u8;

        let half = PAGE_SIZE / 2;
        self.send_hot_payload(io, 0x00, 0x0000, &bytes[..half]).await?;
        self.erase_memory(io, 0x01).await?;
        self.write_flash(io, 0x00, 0x0000, 0x01, 0x0000, half as u16).await?;
        self.send_hot_payload(io, 0x00, 0x0000, &bytes[half..]).await?;
        self.write_flash(io, 0x00, 0x0000, 0x01, half as u16, half as u16).await?;
        Ok(())
    }

    /* ---- Read individual onboard profiles from flash --------------- */

    async fn read_onboard_profile(
        &self, io: &mut DeviceIo, profile_idx: usize,
    ) -> Result<Hidpp10Profile> {
        let mut profile = if profile_idx < self.onboard_profiles.len() {
            self.onboard_profiles[profile_idx].clone()
        } else {
            Hidpp10Profile::default()
        };
        if profile.page == 0 { return Ok(profile); }

        /* Read page, tolerating CRC failures (the mouse still honors the data). */
        let page_data = match self.read_page(io, profile.page).await {
            Ok(data) => data,
            Err(e) => {
                if profile.enabled {
                    warn!("HID++ 1.0: profile {profile_idx} bad CRC, assuming valid: {e}");
                }
                let mut data = [0u8; PAGE_SIZE];
                for i in (0..PAGE_SIZE).step_by(16) {
                    let chunk = self.read_memory(io, profile.page, i as u16).await?;
                    data[i..i + 16].copy_from_slice(&chunk);
                }
                data
            }
        };

        match self.profile_type {
            Hidpp10ProfileType::G500 => {
                /* Layout: RGB(3) + unknown(1) + DPI_16(30) + angle(1) + default(1) +
                 * unknown2(2) + refresh(1) + buttons(39) + metadata(425) = 503 */
                profile.red = page_data[0];
                profile.green = page_data[1];
                profile.blue = page_data[2];
                profile.dpi_modes = parse_dpi_modes_16(&page_data[4..], NUM_DPI_MODES, &self.dpi_table);
                profile.angle_correction = page_data[34] != 0;
                profile.default_dpi_mode = page_data[35];
                let r = page_data[38];
                profile.refresh_rate = if r > 0 { 1000 / u16::from(r) } else { 0 };
                let bstart = 39;
                profile.buttons = Self::parse_buttons(&page_data[bstart..], NUM_BUTTONS);
                profile.name = parse_profile_name(&page_data[78..]);
            }
            Hidpp10ProfileType::G700 => {
                /* Layout: DPI_8dual(20) + default(1) + unknown1(3) + refresh(1) +
                 * unknown2(10) + buttons(39) + metadata(425) = 499 */
                profile.dpi_modes = parse_dpi_modes_8_dual(&page_data[0..], NUM_DPI_MODES, &self.dpi_table);
                profile.default_dpi_mode = page_data[20];
                let r = page_data[24];
                profile.refresh_rate = if r > 0 { 1000 / u16::from(r) } else { 0 };
                let bstart = 35;
                profile.buttons = Self::parse_buttons(&page_data[bstart..], NUM_BUTTONS);
                let meta_start = bstart + NUM_BUTTONS * 3;
                profile.name = parse_profile_name(&page_data[meta_start..]);
            }
            Hidpp10ProfileType::G9 => {
                /* Layout: RGB(3) + unknown(1) + DPI_8(15) + default(1) + unknown2(2) +
                 * refresh(1) + buttons(30) + unknown3(3) + metadata */
                profile.red = page_data[0];
                profile.green = page_data[1];
                profile.blue = page_data[2];
                profile.dpi_modes = parse_dpi_modes_8(&page_data[4..], NUM_DPI_MODES, &self.dpi_table);
                profile.default_dpi_mode = page_data[19];
                let r = page_data[22];
                profile.refresh_rate = if r > 0 { 1000 / u16::from(r) } else { 0 };
                let bstart = 23;
                profile.buttons = Self::parse_buttons(&page_data[bstart..], NUM_BUTTONS_G9);
                let meta_start = bstart + NUM_BUTTONS_G9 * 3 + 3;
                profile.name = parse_profile_name(&page_data[meta_start..]);
            }
            Hidpp10ProfileType::Unknown => {}
        }

        /* Read macros for any button bound to a macro address. The macro
         * Vec is indexed by button number; entries that are not macros
         * are stored as None. */
        profile.macros = Vec::with_capacity(profile.buttons.len());
        for binding in &profile.buttons {
            match *binding {
                Hidpp10ButtonBinding::Macro { page, offset } => {
                    let byte_offset = u8::from(offset).wrapping_mul(2);
                    match self.read_macro(io, page, byte_offset).await {
                        Ok(events) => {
                            debug!(
                                "HID++ 1.0: read macro at ({page:02X},{offset:02X}): {} events",
                                events.len()
                            );
                            profile.macros.push(Some(events));
                        }
                        Err(e) => {
                            warn!("HID++ 1.0: failed to read macro at ({page:02X},{offset:02X}): {e}");
                            profile.macros.push(None);
                        }
                    }
                }
                _ => profile.macros.push(None),
            }
        }

        Ok(profile)
    }

    fn parse_buttons(data: &[u8], count: usize) -> Vec<Hidpp10ButtonBinding> {
        (0..count)
            .filter_map(|i| {
                let off = i * 3;
                if off + 3 > data.len() { return None; }
                Some(parse_button_binding(&[data[off], data[off + 1], data[off + 2]]))
            })
            .collect()
    }

    /* ---- Write a profile to flash ---------------------------------- */

    async fn write_onboard_profile(
        &self, io: &mut DeviceIo, profile_idx: usize, profile: &Hidpp10Profile,
    ) -> Result<()> {
        if self.profile_type == Hidpp10ProfileType::Unknown || profile.page == 0 {
            return Ok(());
        }

        /* Read existing page to preserve unknown fields. */
        let mut page_data = [0xFFu8; PAGE_SIZE];
        match self.read_page(io, profile.page).await {
            Ok(d) => page_data = d,
            Err(_) => {
                for i in (0..PAGE_SIZE).step_by(16) {
                    if let Ok(chunk) = self.read_memory(io, profile.page, i as u16).await {
                        page_data[i..i + 16].copy_from_slice(&chunk);
                    }
                }
            }
        }

        match self.profile_type {
            Hidpp10ProfileType::G500 => {
                page_data[0] = profile.red;
                page_data[1] = profile.green;
                page_data[2] = profile.blue;
                self.write_dpi_modes_16(&mut page_data[4..], &profile.dpi_modes);
                page_data[34] = u8::from(profile.angle_correction);
                page_data[35] = profile.default_dpi_mode;
                page_data[38] = Self::rate_to_byte(profile.refresh_rate);
                Self::write_buttons(&mut page_data[39..], &profile.buttons);
            }
            Hidpp10ProfileType::G700 => {
                self.write_dpi_modes_8_dual(&mut page_data[0..], &profile.dpi_modes);
                page_data[20] = profile.default_dpi_mode;
                page_data[24] = Self::rate_to_byte(profile.refresh_rate);
                Self::write_buttons(&mut page_data[35..], &profile.buttons);
            }
            Hidpp10ProfileType::G9 => {
                page_data[0] = profile.red;
                page_data[1] = profile.green;
                page_data[2] = profile.blue;
                self.write_dpi_modes_8(&mut page_data[4..], &profile.dpi_modes);
                page_data[19] = profile.default_dpi_mode;
                page_data[22] = Self::rate_to_byte(profile.refresh_rate);
                Self::write_buttons(&mut page_data[23..], &profile.buttons);
            }
            Hidpp10ProfileType::Unknown => return Ok(()),
        }

        let crc = hidpp::compute_ccitt_crc(&page_data[..PAGE_SIZE - 2]);
        page_data[PAGE_SIZE - 2] = (crc >> 8) as u8;
        page_data[PAGE_SIZE - 1] = (crc & 0xFF) as u8;

        /* Atomic write: factory profile → upload → erase → flash → restore. */
        self.set_register(
            io, REG_CURRENT_PROFILE, [PROFILE_TYPE_FACTORY, 0x00, 0x00],
        ).await.context("Failed to switch to factory profile for write")?;

        let half = PAGE_SIZE / 2;
        self.send_hot_payload(io, 0x00, 0x0000, &page_data[..half]).await?;
        self.erase_memory(io, profile.page).await?;
        self.write_flash(io, 0x00, 0x0000, profile.page, 0x0000, half as u16).await?;
        self.send_hot_payload(io, 0x00, 0x0000, &page_data[half..]).await?;
        self.write_flash(io, 0x00, 0x0000, profile.page, half as u16, half as u16).await?;

        self.set_register(
            io, REG_CURRENT_PROFILE,
            [PROFILE_TYPE_INDEX, profile_idx as u8, 0x00],
        ).await.context("Failed to restore profile after write")?;
        Ok(())
    }

    fn rate_to_byte(rate: u16) -> u8 {
        if rate > 0 {
            (1000u32 / u32::from(rate)).min(u32::from(u8::MAX)) as u8
        } else {
            0
        }
    }

    /* DPI mode serialization helpers */

    fn write_dpi_modes_16(&self, data: &mut [u8], modes: &[Hidpp10DpiMode]) {
        for (i, mode) in modes.iter().enumerate().take(NUM_DPI_MODES) {
            let base = i * 6;
            if base + 6 > data.len() { break; }
            data[base..base + 2].copy_from_slice(&u16::from(raw_from_dpi(&self.dpi_table, mode.xres)).to_be_bytes());
            data[base + 2..base + 4].copy_from_slice(&u16::from(raw_from_dpi(&self.dpi_table, mode.yres)).to_be_bytes());
            data[base + 4] = Self::led_nibble_pair(mode.leds[0], mode.leds[1]);
            data[base + 5] = Self::led_nibble_pair(mode.leds[2], mode.leds[3]);
        }
    }

    fn write_dpi_modes_8_dual(&self, data: &mut [u8], modes: &[Hidpp10DpiMode]) {
        for (i, mode) in modes.iter().enumerate().take(NUM_DPI_MODES) {
            let base = i * 4;
            if base + 4 > data.len() { break; }
            data[base] = raw_from_dpi(&self.dpi_table, mode.xres);
            data[base + 1] = raw_from_dpi(&self.dpi_table, mode.yres);
            data[base + 2] = Self::led_nibble_pair(mode.leds[0], mode.leds[1]);
            data[base + 3] = Self::led_nibble_pair(mode.leds[2], mode.leds[3]);
        }
    }

    fn write_dpi_modes_8(&self, data: &mut [u8], modes: &[Hidpp10DpiMode]) {
        for (i, mode) in modes.iter().enumerate().take(NUM_DPI_MODES) {
            let base = i * 3;
            if base + 3 > data.len() { break; }
            data[base] = raw_from_dpi(&self.dpi_table, mode.xres);
            data[base + 1] = Self::led_nibble_pair(mode.leds[0], mode.leds[1]);
            data[base + 2] = Self::led_nibble_pair(mode.leds[2], mode.leds[3]);
        }
    }

    fn led_nibble_pair(lo: bool, hi: bool) -> u8 {
        (if lo { 0x02 } else { 0x01 }) | (if hi { 0x20 } else { 0x10 })
    }

    fn write_buttons(data: &mut [u8], buttons: &[Hidpp10ButtonBinding]) {
        for (i, binding) in buttons.iter().enumerate() {
            let base = i * 3;
            if base + 3 > data.len() { break; }
            data[base..base + 3].copy_from_slice(&serialize_button_binding(binding));
        }
    }

    /* ---- Bridge onboard profiles into the DeviceInfo model --------- */

    fn apply_onboard_profiles_to_device_info(
        &self, info: &mut DeviceInfo, active_idx: u32,
    ) {
        for (i, onboard) in self.onboard_profiles.iter().enumerate() {
            if i >= info.profiles.len() { break; }
            let profile = &mut info.profiles[i];
            profile.is_active = (i as u32) == active_idx;
            profile.is_enabled = onboard.enabled;
            if !onboard.name.is_empty() {
                profile.name.clone_from(&onboard.name);
            }
            profile.report_rate = u32::from(onboard.refresh_rate);

            for (j, dm) in onboard.dpi_modes.iter().enumerate() {
                if j >= profile.resolutions.len() { continue; }
                let res = &mut profile.resolutions[j];
                res.dpi = if dm.xres == dm.yres {
                    Dpi::Unified(dm.xres)
                } else {
                    Dpi::Separate { x: dm.xres, y: dm.yres }
                };
                res.is_default = j as u8 == onboard.default_dpi_mode;
                if profile.is_active && res.is_default {
                    res.is_active = true;
                }
            }

            for (j, binding) in onboard.buttons.iter().enumerate() {
                if j >= profile.buttons.len() { continue; }
                let btn = &mut profile.buttons[j];
                match *binding {
                    Hidpp10ButtonBinding::Button { button } => {
                        btn.action_type = ActionType::Button;
                        btn.mapping_value = u32::from(button);
                    }
                    Hidpp10ButtonBinding::Keys { key, .. } => {
                        btn.action_type = ActionType::Key;
                        btn.mapping_value = u32::from(key);
                    }
                    Hidpp10ButtonBinding::Special { code } => {
                        btn.action_type = ActionType::Special;
                        btn.mapping_value = hidpp10_special_from_code(code as u8);
                    }
                    Hidpp10ButtonBinding::ConsumerControl { usage } => {
                        btn.action_type = ActionType::Key;
                        btn.mapping_value = u32::from(usage);
                    }
                    Hidpp10ButtonBinding::Disabled => {
                        btn.action_type = ActionType::None;
                    }
                    Hidpp10ButtonBinding::Macro { .. } => {
                        btn.action_type = ActionType::Macro;
                    }
                    Hidpp10ButtonBinding::Unknown { .. } => {
                        btn.action_type = ActionType::Unknown;
                    }
                }
            }

            /* Populate LED color from profile RGB (G700 has no RGB LEDs). */
            if !profile.leds.is_empty() && self.profile_type != Hidpp10ProfileType::G700 {
                let led = &mut profile.leds[0];
                led.color = Color::from_rgb(RgbColor {
                    r: onboard.red, g: onboard.green, b: onboard.blue,
                });
                led.mode = LedMode::Solid;
            }
        }
    }
}

/* ------------------------------------------------------------------ */
/*  DeviceDriver trait implementation                                   */
/* ------------------------------------------------------------------ */

#[async_trait]
impl super::DeviceDriver for Hidpp10Driver {
    fn name(&self) -> &str {
        "Logitech HID++ 1.0"
    }

    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()> {
        const PROBE_INDICES: &[u8] = &[DEVICE_IDX_RECEIVER, DEVICE_IDX_CORDED];
        for &idx in PROBE_INDICES {
            if let Some(params) = self.try_probe_index(io, idx).await {
                self.device_index = idx;
                self.version = ProtocolVersion {
                    major: params[0],
                    minor: params[1],
                };
                info!(
                    "HID++ 1.0 device detected at index 0x{idx:02X} (protocol {}.{})",
                    self.version.major, self.version.minor
                );
                return Ok(());
            }
            debug!("HID++ 1.0 probe at index 0x{idx:02X}: no response");
        }
        anyhow::bail!(
            "HID++ 1.0 protocol version probe failed (tried indices: {:02X?})",
            PROBE_INDICES
        );
    }

    async fn load_profiles(&mut self, io: &mut DeviceIo, info: &mut DeviceInfo) -> Result<()> {
        /* Read onboard profiles from flash if the device supports them. */
        if self.profile_type != Hidpp10ProfileType::Unknown {
            self.read_profile_directory(io).await?;
            let count = self.onboard_profiles.len();
            let mut loaded = Vec::with_capacity(count);
            for i in 0..count {
                match self.read_onboard_profile(io, i).await {
                    Ok(p) => loaded.push(p),
                    Err(e) => {
                        warn!("HID++ 1.0: failed to read onboard profile {i}: {e}");
                        loaded.push(Hidpp10Profile::default());
                    }
                }
            }
            self.onboard_profiles = loaded;
        }

        let active_idx = self.read_current_profile(io).await.unwrap_or_else(|e| {
            warn!("Failed to read current profile: {e}");
            0
        });

        if self.profile_type != Hidpp10ProfileType::Unknown
            && !self.onboard_profiles.is_empty()
        {
            self.apply_onboard_profiles_to_device_info(info, active_idx);
        }

        /* Supplement with live register values. */
        for profile in &mut info.profiles {
            profile.is_active = profile.index == active_idx;
            if let Err(e) = self.read_resolution(io, profile).await {
                warn!("Failed to read DPI for profile {}: {}", profile.index, e);
            }
            if let Err(e) = self.read_refresh_rate(io, profile).await {
                warn!("Failed to read report rate for profile {}: {}", profile.index, e);
            }
            if let Err(e) = self.read_led_color(io, profile).await {
                warn!("Failed to read LED color for profile {}: {}", profile.index, e);
            }
        }

        debug!(
            "HID++ 1.0: loaded {} profiles, active = {active_idx}",
            info.profiles.len()
        );
        Ok(())
    }

    async fn commit(&mut self, io: &mut DeviceIo, info: &DeviceInfo) -> Result<()> {
        if let Some(profile) = info.profiles.iter().find(|p| p.is_active)
            && let Ok(idx) = u8::try_from(profile.index)
        {
            if let Err(e) = self.write_resolution(io, profile).await {
                warn!("Failed to commit DPI for profile {}: {}", profile.index, e);
            }
            if let Err(e) = self.write_refresh_rate(io, profile).await {
                warn!("Failed to commit report rate for profile {}: {}", profile.index, e);
            }
            if let Err(e) = self.write_led_color(io, profile).await {
                warn!("Failed to commit LED color for profile {}: {}", profile.index, e);
            }

            /* Write onboard profile to flash if supported. */
            if self.profile_type != Hidpp10ProfileType::Unknown
                && (idx as usize) < self.onboard_profiles.len()
            {
                let op = self.onboard_profiles[idx as usize].clone();
                if let Err(e) = self.write_onboard_profile(io, idx as usize, &op).await {
                    warn!("Failed to write onboard profile {idx}: {e}");
                }
            }

            self.set_register(io, REG_CURRENT_PROFILE, [idx, 0x00, 0x00])
                .await
                .context("Failed to commit active profile")?;
            debug!("HID++ 1.0: committed active profile = {idx}");
        }
        Ok(())
    }
}
