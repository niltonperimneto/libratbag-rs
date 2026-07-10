/*
 * Asus ROG mouse driver.
 *
 * Ported from the C implementation in libratbag's src/asus.c and
 * src/driver-asus.c; device descriptions live in data/devices/asus-*.device.
 *
 * Protocol: raw HID output/input reports (64 bytes each), no checksum.
 *   Request:  buf[0..2] = command (u16 LE), buf[2..64] = parameters
 *   Response: buf[0..2] = status  (u16 LE), buf[2..64] = result data
 * A status of 0xaaff (ASUS_STATUS_ERROR) means the device is sleeping,
 * disconnected, or rejected the request — wireless mice are registered
 * anyway and recovered on the next commit.
 *
 * Reads:  GET_PROFILE_DATA (active profile, dpi preset, firmware),
 *         GET_BUTTON_DATA (17 bindings per group), GET_SETTINGS (DPI
 *         presets + rate/debounce/snapping, optionally per-axis XY),
 *         GET_LED_DATA (up to 3 LEDs: mode, brightness, RGB).
 * Writes: SET_BUTTON, SET_SETTING, SET_LED, SET_PROFILE, SAVE (persist
 *         the currently selected profile to flash).
 *
 * Per-device quirks (Quirks= in the .device file):
 *   DOUBLE_DPI        wire DPI value is half the real DPI
 *   STRIX_PROFILE     active profile id sits one byte earlier
 *   RAW_BRIGHTNESS    LED brightness is 0-255 instead of 0-4
 *   SEPARATE_XY_DPI   extra GET_SETTINGS query for per-axis DPI
 *   SEPARATE_LEDS     one GET_LED_DATA query per LED index
 *   BUTTONS_SECONDARY second button group (indices 17-33)
 */

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tracing::{debug, warn};

use crate::engine::device::{ActionType, Color, DeviceInfo, Dpi, LedMode, ProfileInfo};
use crate::hal::{DeviceDriver, DeviceIo, DriverError};

// ────────────────────────────── Constants ──────────────────────────────────

const ASUS_PACKET_SIZE: usize = 64;

/* Status code returned by the hardware when the device is sleeping or
 * disconnected (wireless). */
const ASUS_STATUS_ERROR: u16 = 0xaaff;

/* Per-attempt response deadline and attempt budget for query().
 * Wired devices answer within a few milliseconds; 500 ms per attempt with
 * two retries generously covers a congested wireless link while keeping a
 * mute device from blocking the actor for more than ~1.5 s per command. */
const ASUS_QUERY_TIMEOUT:  std::time::Duration = std::time::Duration::from_millis(500);
const ASUS_QUERY_ATTEMPTS: u8 = 3;

/* Command words placed in buf[0..2] of every request packet. */
const ASUS_CMD_GET_LED_DATA:     u16 = 0x0312; /* get all LEDs */
const ASUS_CMD_GET_SETTINGS:     u16 = 0x0412; /* dpi, rate, button response, angle snapping */
const ASUS_CMD_GET_BUTTON_DATA:  u16 = 0x0512; /* get all buttons */
const ASUS_CMD_GET_PROFILE_DATA: u16 = 0x0012; /* get current profile info */
const ASUS_CMD_SET_LED:          u16 = 0x2851; /* set single led */
const ASUS_CMD_SET_SETTING:      u16 = 0x3151; /* dpi / rate / button response / angle snapping */
const ASUS_CMD_SET_BUTTON:       u16 = 0x2151; /* set single button */
const ASUS_CMD_SET_PROFILE:      u16 = 0x0250; /* switch profile */
const ASUS_CMD_SAVE:             u16 = 0x0350; /* save settings */

/* Field selectors for ASUS_CMD_SET_SETTING (added to dpi_count). */
const ASUS_FIELD_RATE:     u8 = 0;
const ASUS_FIELD_RESPONSE: u8 = 1;
const ASUS_FIELD_SNAPPING: u8 = 2;

/* Button action type bytes from the hardware. */
const ASUS_ACTION_TYPE_KEY:      u8 = 0; /* keyboard key */
const ASUS_ACTION_TYPE_BUTTON:   u8 = 1; /* mouse button */
const ASUS_BUTTON_CODE_DISABLED: u8 = 0xff; /* "none" action */

/* Capacity limits. */
const ASUS_MAX_NUM_BUTTON:       usize = 17;
const ASUS_MAX_NUM_BUTTON_GROUP: usize = 2;
const ASUS_MAX_NUM_LED:          usize = 3;
const ASUS_MAX_NUM_LED_MODES:    usize = 7;

/* Quirk bitmasks. */
const ASUS_QUIRK_DOUBLE_DPI:        u32 = 1 << 0;
const ASUS_QUIRK_STRIX_PROFILE:     u32 = 1 << 1;
#[allow(dead_code)]
const ASUS_QUIRK_BATTERY_V2:        u32 = 1 << 2; /* unused in probe/commit, reserved */
const ASUS_QUIRK_RAW_BRIGHTNESS:    u32 = 1 << 3;
const ASUS_QUIRK_SEPARATE_XY_DPI:   u32 = 1 << 4;
const ASUS_QUIRK_SEPARATE_LEDS:     u32 = 1 << 5;
const ASUS_QUIRK_BUTTONS_SECONDARY: u32 = 1 << 6;

/* Fixed hardware capability lists. */
static ASUS_POLLING_RATES:  &[u32] = &[125, 250, 500, 1000];
static ASUS_DEBOUNCE_TIMES: &[u32] = &[4, 8, 12, 16, 20, 24, 28, 32];

/* Default button-mapping (ASUS hardware code for each button slot).
 * Values that stay -1 after init_from_config mean "unused slot". */
static ASUS_DEFAULT_BUTTON_MAPPING: &[u8] = &[
    0xf0, /* left */
    0xf1, /* right */
    0xf2, /* middle */
    0xe4, /* backward */
    0xe5, /* forward */
    0xe6, /* DPI cycle */
    0xe8, /* wheel up */
    0xe9, /* wheel down */
];

/* Default ASUS hardware mode-index → LedMode mapping.
 *   0 = solid, 1 = breathing, 2 = cycle, 3..6 = solid (wave/reactive/custom/battery).
 * Device files may override individual entries via LedModes=. */
const ASUS_DEFAULT_LED_MODES: [LedMode; ASUS_MAX_NUM_LED_MODES] = [
    LedMode::Solid,
    LedMode::Breathing,
    LedMode::Cycle,
    LedMode::Solid,
    LedMode::Solid,
    LedMode::Solid,
    LedMode::Solid,
];

// ─────────────────────────── Button tables ─────────────────────────────────

/* Ratbag-side action kind for an entry in the ASUS button table. */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AsusButtonKind {
    None,
    Button(u32),   /* ActionType::Button, value = ratbag button index (1-based) */
    Special(u32),  /* ActionType::Special, value = special action constant below */
    Joystick,      /* joystick axis — also treated as Special in DBus */
}

struct AsusButtonEntry {
    asus_code: u8,
    kind: AsusButtonKind,
}

/* Special action values: re-export from the shared canonical module so that
 * all drivers and DBus clients agree on the same numeric encoding (matching
 * the C libratbag `ratbag_button_action_special` enum, base = 1 << 30). */
use crate::engine::device::special_action;
const SPECIAL_WHEEL_UP:      u32 = special_action::WHEEL_UP;
const SPECIAL_WHEEL_DOWN:    u32 = special_action::WHEEL_DOWN;
const SPECIAL_WHEEL_RIGHT:   u32 = special_action::WHEEL_RIGHT;
const SPECIAL_WHEEL_LEFT:    u32 = special_action::WHEEL_LEFT;
const SPECIAL_RES_CYCLE_UP:  u32 = special_action::RESOLUTION_CYCLE_UP;
const SPECIAL_RES_ALTERNATE: u32 = special_action::RESOLUTION_ALTERNATE;

/* This table mirrors ASUS_BUTTON_MAPPING[] from asus.h, translated into
 * idiomatic Rust. Order is intentional: the C code iterates in order to
 * build button_indices[], and find_button_by_action() depends on ordering
 * to pick the non-joystick variant first when is_joystick=false. */
static ASUS_BUTTON_MAPPING: &[AsusButtonEntry] = &[
    AsusButtonEntry { asus_code: 0xf0, kind: AsusButtonKind::Button(1) },  /* left */
    AsusButtonEntry { asus_code: 0xf1, kind: AsusButtonKind::Button(2) },  /* right */
    AsusButtonEntry { asus_code: 0xf2, kind: AsusButtonKind::Button(3) },  /* middle */
    AsusButtonEntry { asus_code: 0xe8, kind: AsusButtonKind::Special(SPECIAL_WHEEL_UP) },
    AsusButtonEntry { asus_code: 0xe9, kind: AsusButtonKind::Special(SPECIAL_WHEEL_DOWN) },
    AsusButtonEntry { asus_code: 0xe6, kind: AsusButtonKind::Special(SPECIAL_RES_CYCLE_UP) },
    AsusButtonEntry { asus_code: 0xe4, kind: AsusButtonKind::Button(4) },   /* backward, left side */
    AsusButtonEntry { asus_code: 0xe5, kind: AsusButtonKind::Button(5) },   /* forward, left side */
    AsusButtonEntry { asus_code: 0xe1, kind: AsusButtonKind::Button(4) },   /* backward, right side */
    AsusButtonEntry { asus_code: 0xe2, kind: AsusButtonKind::Button(5) },   /* forward, right side */
    AsusButtonEntry { asus_code: 0xe7, kind: AsusButtonKind::Special(SPECIAL_RES_ALTERNATE) },
    AsusButtonEntry { asus_code: 0xea, kind: AsusButtonKind::None },  /* side button A */
    AsusButtonEntry { asus_code: 0xeb, kind: AsusButtonKind::None },  /* side button B */
    AsusButtonEntry { asus_code: 0xec, kind: AsusButtonKind::None },  /* side button C */
    AsusButtonEntry { asus_code: 0xed, kind: AsusButtonKind::None },  /* side button D */
    AsusButtonEntry { asus_code: 0xee, kind: AsusButtonKind::None },  /* side button E */
    AsusButtonEntry { asus_code: 0xef, kind: AsusButtonKind::None },  /* side button F */
    AsusButtonEntry { asus_code: 0xd0, kind: AsusButtonKind::Joystick },    /* joystick up */
    AsusButtonEntry { asus_code: 0xd1, kind: AsusButtonKind::Joystick },    /* joystick down */
    AsusButtonEntry { asus_code: 0xd2, kind: AsusButtonKind::Joystick },    /* joystick forward */
    AsusButtonEntry { asus_code: 0xd3, kind: AsusButtonKind::Joystick },    /* joystick backward */
    AsusButtonEntry { asus_code: 0xd7, kind: AsusButtonKind::Special(SPECIAL_WHEEL_DOWN) },  /* axis -Y */
    AsusButtonEntry { asus_code: 0xd8, kind: AsusButtonKind::Special(SPECIAL_WHEEL_UP) },    /* axis +Y */
    AsusButtonEntry { asus_code: 0xda, kind: AsusButtonKind::Special(SPECIAL_WHEEL_RIGHT) }, /* axis -X */
    AsusButtonEntry { asus_code: 0xdb, kind: AsusButtonKind::Special(SPECIAL_WHEEL_LEFT) },  /* axis +X */
];

static ASUS_JOYSTICK_CODES: &[u8] = &[0xd0, 0xd1, 0xd2, 0xd3, 0xd7, 0xd8, 0xda, 0xdb];

// ─────────────────────────── Key-code table ────────────────────────────────

/* Linux input event codes for key actions (from linux/input-event-codes.h).
 * These are the evdev scancode values, matching what libratbag uses. */
const KEY_ESC:       u32 = 1;
const KEY_1:         u32 = 2;
const KEY_2:         u32 = 3;
const KEY_3:         u32 = 4;
const KEY_4:         u32 = 5;
const KEY_5:         u32 = 6;
const KEY_6:         u32 = 7;
const KEY_7:         u32 = 8;
const KEY_8:         u32 = 9;
const KEY_9:         u32 = 10;
const KEY_0:         u32 = 11;
const KEY_MINUS:     u32 = 12;
const KEY_EQUAL:     u32 = 13;
const KEY_BACKSPACE: u32 = 14;
const KEY_TAB:       u32 = 15;
const KEY_Q:         u32 = 16;
const KEY_W:         u32 = 17;
const KEY_E:         u32 = 18;
const KEY_R:         u32 = 19;
const KEY_T:         u32 = 20;
const KEY_Y:         u32 = 21;
const KEY_U:         u32 = 22;
const KEY_I:         u32 = 23;
const KEY_O:         u32 = 24;
const KEY_P:         u32 = 25;
const KEY_A:         u32 = 30;
const KEY_S:         u32 = 31;
const KEY_D:         u32 = 32;
const KEY_F:         u32 = 33;
const KEY_G:         u32 = 34;
const KEY_H:         u32 = 35;
const KEY_J:         u32 = 36;
const KEY_K:         u32 = 37;
const KEY_L:         u32 = 38;
const KEY_GRAVE:     u32 = 41;
const KEY_Z:         u32 = 44;
const KEY_X:         u32 = 45;
const KEY_C:         u32 = 46;
const KEY_V:         u32 = 47;
const KEY_B:         u32 = 48;
const KEY_N:         u32 = 49;
const KEY_M:         u32 = 50;
const KEY_SLASH:     u32 = 53;
const KEY_SPACE:     u32 = 57;
const KEY_F1:        u32 = 59;
const KEY_F2:        u32 = 60;
const KEY_F3:        u32 = 61;
const KEY_F4:        u32 = 62;
const KEY_F5:        u32 = 63;
const KEY_F6:        u32 = 64;
const KEY_F7:        u32 = 65;
const KEY_F8:        u32 = 66;
const KEY_F9:        u32 = 67;
const KEY_F10:       u32 = 68;
const KEY_KP7:       u32 = 71;
const KEY_KP8:       u32 = 72;
const KEY_KP9:       u32 = 73;
const KEY_KP4:       u32 = 75;
const KEY_KP5:       u32 = 76;
const KEY_KP6:       u32 = 77;
const KEY_KPPLUS:    u32 = 78;
const KEY_KP1:       u32 = 79;
const KEY_KP2:       u32 = 80;
const KEY_KP3:       u32 = 81;
const KEY_F11:       u32 = 87;
const KEY_F12:       u32 = 88;
const KEY_UP:        u32 = 103;
const KEY_PAGEUP:    u32 = 104;
const KEY_LEFT:      u32 = 105;
const KEY_RIGHT:     u32 = 106;
const KEY_DOWN:      u32 = 108;
const KEY_PAGEDOWN:  u32 = 109;
const KEY_DELETE:    u32 = 111;
const KEY_HOME:      u32 = 102;
const KEY_ENTER:     u32 = 28;

/* ASUS key-code table: index = ASUS code, value = Linux evdev code, 0 = unmapped.
 * Mirrors ASUS_KEY_MAPPING[] in asus.c exactly (99 entries, 0x00–0x62). */
static ASUS_KEY_MAPPING: &[u32] = &[
    /* 0x00 */ 0,         0,         0,         0,
    /* 0x04 */ KEY_A,     KEY_B,     KEY_C,     KEY_D,
    /* 0x08 */ KEY_E,     KEY_F,     KEY_G,     KEY_H,
    /* 0x0C */ KEY_I,     KEY_J,     KEY_K,     KEY_L,
    /* 0x10 */ KEY_M,     KEY_N,     KEY_O,     KEY_P,
    /* 0x14 */ KEY_Q,     KEY_R,     KEY_S,     KEY_T,
    /* 0x18 */ KEY_U,     KEY_V,     KEY_W,     KEY_X,
    /* 0x1C */ KEY_Y,     KEY_Z,     KEY_1,     KEY_2,
    /* 0x20 */ KEY_3,     KEY_4,     KEY_5,     KEY_6,
    /* 0x24 */ KEY_7,     KEY_8,     KEY_9,     KEY_0,
    /* 0x28 */ KEY_ENTER, KEY_ESC,   KEY_BACKSPACE, KEY_TAB,
    /* 0x2C */ KEY_SPACE, KEY_MINUS, KEY_KPPLUS, 0,
    /* 0x30 */ 0,         0,         0,         0,
    /* 0x34 */ 0,         KEY_GRAVE, KEY_EQUAL, 0,
    /* 0x38 */ KEY_SLASH, 0,         KEY_F1,    KEY_F2,
    /* 0x3C */ KEY_F3,    KEY_F4,    KEY_F5,    KEY_F6,
    /* 0x40 */ KEY_F7,    KEY_F8,    KEY_F9,    KEY_F10,
    /* 0x44 */ KEY_F11,   KEY_F12,   0,         0,
    /* 0x48 */ 0,         0,         KEY_HOME,  KEY_PAGEUP,
    /* 0x4C */ KEY_DELETE, 0,        KEY_PAGEDOWN, KEY_RIGHT,
    /* 0x50 */ KEY_LEFT,  KEY_DOWN,  KEY_UP,    0,
    /* 0x54 */ 0,         0,         0,         0,
    /* 0x58 */ 0,         KEY_KP1,   KEY_KP2,   KEY_KP3,
    /* 0x5C */ KEY_KP4,   KEY_KP5,   KEY_KP6,   KEY_KP7,
    /* 0x60 */ KEY_KP8,   KEY_KP9,   0,
];

// ────────────────────── Pure helper functions ───────────────────────────────

/// Parse quirk strings from `DriverConfig.quirks` into a bitmask.
fn parse_quirks(quirk_strings: &[String]) -> u32 {
    let mut q = 0u32;
    for s in quirk_strings {
        match s.as_str() {
            "DOUBLE_DPI"        => q |= ASUS_QUIRK_DOUBLE_DPI,
            "STRIX_PROFILE"     => q |= ASUS_QUIRK_STRIX_PROFILE,
            "BATTERY_V2"        => q |= ASUS_QUIRK_BATTERY_V2,
            "RAW_BRIGHTNESS"    => q |= ASUS_QUIRK_RAW_BRIGHTNESS,
            "SEPARATE_XY_DPI"   => q |= ASUS_QUIRK_SEPARATE_XY_DPI,
            "SEPARATE_LEDS"     => q |= ASUS_QUIRK_SEPARATE_LEDS,
            "BUTTONS_SECONDARY" => q |= ASUS_QUIRK_BUTTONS_SECONDARY,
            other => warn!("ASUS: unknown quirk string: {}", other),
        }
    }
    q
}

/// Convert the stored hardware DPI byte to the user-facing DPI value.
/// Formula: stored * 50 + 50, then × 2 if DOUBLE_DPI.
fn dpi_from_stored(stored: u16, quirks: u32) -> u32 {
    let mut val = (stored as u32) * 50 + 50;
    if quirks & ASUS_QUIRK_DOUBLE_DPI != 0 {
        val *= 2;
    }
    val
}

/// Convert the user-facing DPI value back to the hardware byte.
///
/// The wire encoding is a single byte (`stored = (dpi - 50) / 50`), so the
/// highest settable DPI is 12,800 — 25,600 with DOUBLE_DPI — even on devices
/// whose sensor advertises more (e.g. Chakram X at 36,000).  The same limit
/// exists in the C driver; values beyond it are clamped.
fn dpi_to_stored(dpi: u32, quirks: u32) -> u8 {
    let adjusted = if quirks & ASUS_QUIRK_DOUBLE_DPI != 0 {
        dpi / 2
    } else {
        dpi
    };
    (adjusted.saturating_sub(50) / 50).min(255) as u8
}

/// Highest DPI expressible in the one-byte wire encoding (see `dpi_to_stored`).
fn max_wire_dpi(quirks: u32) -> u32 {
    let base = 255 * 50 + 50;
    if quirks & ASUS_QUIRK_DOUBLE_DPI != 0 { base * 2 } else { base }
}

/// Convert the hardware brightness byte to the ratbag 0-255 scale.
/// Non-raw: hardware uses 0-4, ratbag uses 0-256 (4 × 64 = 256).
/// RAW_BRIGHTNESS: byte is passed through directly.
fn brightness_to_ratbag(raw: u8, quirks: u32) -> u32 {
    if quirks & ASUS_QUIRK_RAW_BRIGHTNESS != 0 {
        raw as u32
    } else {
        (raw as u32).saturating_mul(64)
    }
}

/// Convert the ratbag 0-255 brightness to the hardware byte.
fn brightness_to_asus(ratbag: u32, quirks: u32) -> u8 {
    if quirks & ASUS_QUIRK_RAW_BRIGHTNESS != 0 {
        ratbag.min(255) as u8
    } else {
        /* Round to nearest step of 64, clamp to 0-4. */
        ((ratbag + 32) / 64).min(4) as u8
    }
}

/// Find a button entry by ASUS hardware code.
fn find_button_by_code(code: u8) -> Option<&'static AsusButtonEntry> {
    ASUS_BUTTON_MAPPING.iter().find(|e| e.asus_code == code)
}

/// Find a button entry matching a ratbag action.
///
/// `is_joystick` restricts the search to joystick codes (or non-joystick) to
/// keep the two ranges mutually exclusive when looking up by action value.
fn find_button_by_action(
    action_type: ActionType,
    value: u32,
    is_joystick: bool,
) -> Option<&'static AsusButtonEntry> {
    ASUS_BUTTON_MAPPING.iter().find(|e| {
        let code_is_joy = is_joystick_code(e.asus_code);
        if is_joystick != code_is_joy {
            return false;
        }
        match (action_type, &e.kind) {
            (ActionType::Button, AsusButtonKind::Button(n)) => *n == value,
            (ActionType::Special, AsusButtonKind::Special(n)) => *n == value,
            _ => false,
        }
    })
}

/// Translate an ASUS key code to the Linux evdev input code.
fn get_linux_key_code(asus_code: u8) -> Option<u32> {
    let val = ASUS_KEY_MAPPING.get(asus_code as usize).copied().unwrap_or(0);
    if val == 0 { None } else { Some(val) }
}

/// Translate a Linux evdev input code to the ASUS key code.
fn find_key_code(linux_code: u32) -> Option<u8> {
    ASUS_KEY_MAPPING
        .iter()
        .position(|&k| k == linux_code)
        .map(|i| i as u8)
}

/// Returns true when the ASUS code belongs to the joystick axis sub-system.
fn is_joystick_code(code: u8) -> bool {
    ASUS_JOYSTICK_CODES.contains(&code)
}

/// Classify a query error as "device asleep / not reachable right now".
///
/// A sleeping or disconnected wireless mouse either answers with
/// ASUS_STATUS_ERROR (surfaced as `DriverError::ProtocolError` — the only
/// place this driver raises it) or stops answering entirely (surfaced as
/// `DriverError::Timeout`).  Anything else — open/read/write failures on the
/// hidraw node — is a hard transport error and must not be masked.
fn is_asleep_error(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<DriverError>(),
        Some(DriverError::ProtocolError { .. } | DriverError::Timeout { .. })
    )
}

/// Return the index into `ASUS_POLLING_RATES` for a given Hz value.
fn polling_rate_index(hz: u32) -> Option<u8> {
    ASUS_POLLING_RATES.iter().position(|&r| r == hz).map(|i| i as u8)
}

/// Return the index into `ASUS_DEBOUNCE_TIMES` for a given millisecond value.
fn debounce_index(ms: u32) -> Option<u8> {
    ASUS_DEBOUNCE_TIMES.iter().position(|&d| d == ms).map(|i| i as u8)
}

/// Parse a LED mode string from a `.device` file `LedModes=` field.
fn parse_led_mode_str(s: &str) -> LedMode {
    if s.eq_ignore_ascii_case("ON") || s.eq_ignore_ascii_case("SOLID") {
        LedMode::Solid
    } else if s.eq_ignore_ascii_case("BREATHING") {
        LedMode::Breathing
    } else if s.eq_ignore_ascii_case("CYCLE") {
        LedMode::Cycle
    } else if s.eq_ignore_ascii_case("OFF") {
        LedMode::Off
    } else if s.eq_ignore_ascii_case("COLORWAVE") {
        LedMode::ColorWave
    } else {
        warn!("ASUS: unknown LED mode string: {}", s);
        LedMode::Solid
    }
}

// ─────────────────────────── Packet types ──────────────────────────────────

/* All ASUS requests are 64-byte raw HID output reports. */
struct AsusRequest {
    buf: [u8; ASUS_PACKET_SIZE],
}

impl AsusRequest {
    /* Build a zeroed request with the command word pre-filled. */
    fn new(cmd: u16) -> Self {
        let mut r = Self { buf: [0u8; ASUS_PACKET_SIZE] };
        r.buf[0..2].copy_from_slice(&cmd.to_le_bytes());
        r
    }

    /* Set a parameter byte at offset `idx` within the params region.
     * params[idx] = buf[2 + idx]; silently ignored if idx is out of range. */
    fn set_param(&mut self, idx: usize, val: u8) {
        if let Some(p) = self.buf.get_mut(2 + idx) {
            *p = val;
        }
    }
}

/* All ASUS responses are 64-byte raw HID input reports. */
struct AsusResponse {
    buf: [u8; ASUS_PACKET_SIZE],
}

impl Default for AsusResponse {
    fn default() -> Self {
        Self { buf: [0u8; ASUS_PACKET_SIZE] }
    }
}

impl AsusResponse {
    fn status_code(&self) -> u16 {
        u16::from_le_bytes([self.buf[0], self.buf[1]])
    }

    /* Access results[idx] = buf[2 + idx].  Returns 0 for out-of-range indices. */
    fn result(&self, idx: usize) -> u8 {
        self.buf.get(2 + idx).copied().unwrap_or(0)
    }
}

/* Parsed button binding for a single button slot. */
#[derive(Clone, Copy, Default)]
struct AsusBinding {
    action: u8,
    type_:  u8,
}

/* All button bindings for one group (primary or secondary). */
struct AsusBindingData {
    bindings: [AsusBinding; ASUS_MAX_NUM_BUTTON],
}

impl AsusBindingData {
    /* Parse from a full response packet.
     *
     * Wire layout (matches `_asus_binding_data` overlaid on `union asus_response`):
     *   response.raw[0..1] = status, [2..5] = pad (4 bytes from raw[0]),
     *   raw[4] = binding[0].action, raw[5] = binding[0].type_, … each 2 bytes.
     *
     * In result() terms (result(i) = buf[2+i]):
     *   binding[k].action = result(2 + k*2)
     *   binding[k].type_  = result(3 + k*2)
     */
    fn from_response(resp: &AsusResponse) -> Self {
        let mut data = Self {
            bindings: [AsusBinding::default(); ASUS_MAX_NUM_BUTTON],
        };
        for k in 0..ASUS_MAX_NUM_BUTTON {
            data.bindings[k].action = resp.result(2 + k * 2);
            data.bindings[k].type_  = resp.result(3 + k * 2);
        }
        data
    }
}

/* Parsed DPI/settings data (2-DPI variant). */
struct AsusDpi2Data {
    dpi:          [u16; 2],
    rate_idx:     u16,
    response_idx: u16,
    snapping:     u16,
}

impl AsusDpi2Data {
    /* Wire layout (matches `_asus_dpi2_data` overlaid on response at raw[0]):
     *   raw[0..3]=pad, raw[4..5]=dpi[0], raw[6..7]=dpi[1],
     *   raw[8..9]=rate, raw[10..11]=response, raw[12..13]=snapping
     * result(i) = raw[i+2], so raw[4] = result(2), raw[5] = result(3), ...
     */
    fn from_response(resp: &AsusResponse) -> Self {
        Self {
            dpi: [
                u16::from_le_bytes([resp.result(2), resp.result(3)]),
                u16::from_le_bytes([resp.result(4), resp.result(5)]),
            ],
            rate_idx:     u16::from_le_bytes([resp.result(6),  resp.result(7)]),
            response_idx: u16::from_le_bytes([resp.result(8),  resp.result(9)]),
            snapping:     u16::from_le_bytes([resp.result(10), resp.result(11)]),
        }
    }
}

/* Parsed DPI/settings data (4-DPI variant). */
struct AsusDpi4Data {
    dpi:          [u16; 4],
    rate_idx:     u16,
    response_idx: u16,
    snapping:     u16,
}

impl AsusDpi4Data {
    /* Wire layout (matches `_asus_dpi4_data` overlaid on response at raw[0]):
     *   raw[0..3]=pad, raw[4..5]=dpi[0], …, raw[10..11]=dpi[3],
     *   raw[12..13]=rate, raw[14..15]=response, raw[16..17]=snapping
     */
    fn from_response(resp: &AsusResponse) -> Self {
        Self {
            dpi: [
                u16::from_le_bytes([resp.result(2),  resp.result(3)]),
                u16::from_le_bytes([resp.result(4),  resp.result(5)]),
                u16::from_le_bytes([resp.result(6),  resp.result(7)]),
                u16::from_le_bytes([resp.result(8),  resp.result(9)]),
            ],
            rate_idx:     u16::from_le_bytes([resp.result(10), resp.result(11)]),
            response_idx: u16::from_le_bytes([resp.result(12), resp.result(13)]),
            snapping:     u16::from_le_bytes([resp.result(14), resp.result(15)]),
        }
    }
}

/* Parsed separate-X/Y DPI data (4 presets). */
struct AsusDpiXyData {
    dpi: [(u16, u16); 4], /* (x, y) pairs */
}

impl AsusDpiXyData {
    /* Wire layout (`_asus_dpi_xy_data`):
     *   raw[0..3]=pad, raw[4..5]=xy[0].x, raw[6..7]=xy[0].y,
     *   raw[8..9]=xy[1].x, …  each pair is 4 bytes.
     */
    fn from_response(resp: &AsusResponse) -> Self {
        let mut dpi = [(0u16, 0u16); 4];
        for i in 0..4 {
            let base = 2 + i * 4;
            dpi[i] = (
                u16::from_le_bytes([resp.result(base),     resp.result(base + 1)]),
                u16::from_le_bytes([resp.result(base + 2), resp.result(base + 3)]),
            );
        }
        Self { dpi }
    }
}

/* Parsed LED entry for a single LED. */
#[derive(Clone, Copy, Default)]
struct AsusLedEntry {
    mode:       u8,
    brightness: u8,
    r:          u8,
    g:          u8,
    b:          u8,
}

/* Parsed LED data for all LEDs returned in one response. */
struct AsusLedData {
    leds: [AsusLedEntry; ASUS_MAX_NUM_LED],
}

impl AsusLedData {
    /* Wire layout (`_asus_led_data`):
     *   raw[0..3]=pad, raw[4]=led[0].mode, raw[5]=led[0].brightness, raw[6..8]=rgb,
     *   raw[9]=led[1].mode, …  each LED is 5 bytes.
     */
    fn from_response(resp: &AsusResponse) -> Self {
        let mut leds = [AsusLedEntry::default(); ASUS_MAX_NUM_LED];
        for i in 0..ASUS_MAX_NUM_LED {
            let base = 2 + i * 5; /* result(2) = raw[4] */
            leds[i] = AsusLedEntry {
                mode:       resp.result(base),
                brightness: resp.result(base + 1),
                r:          resp.result(base + 2),
                g:          resp.result(base + 3),
                b:          resp.result(base + 4),
            };
        }
        Self { leds }
    }
}

/* Intermediate struct for returning profile-discovery results. */
struct AsusProfileInfo {
    profile_id:         u32,
    dpi_preset:         Option<u32>,
    firmware_primary:   (u8, u8, u8), /* major, minor, build */
    firmware_secondary: (u8, u8, u8),
}

// ────────────────────────── Driver struct ──────────────────────────────────

/// Asus ROG mouse driver.
pub struct AsusDriver {
    /* true once a successful hardware query has completed. */
    is_ready: bool,

    /* Flat mapping array: indices 0..17 = primary group, 17..34 = secondary.
     * `None` means "this slot is unused". */
    button_mapping: [Option<u8>; ASUS_MAX_NUM_BUTTON * ASUS_MAX_NUM_BUTTON_GROUP],

    /* For ButtonInfo at DeviceInfo index N: button_indices[N] = the flat
     * position in button_mapping to use (`None` = no mapping). */
    button_indices: [Option<usize>; ASUS_MAX_NUM_BUTTON * ASUS_MAX_NUM_BUTTON_GROUP],

    /* ASUS hardware mode index (0-6) → LedMode.  Overridden per device from
     * the LedModes= field in the .device file. */
    led_modes: [LedMode; ASUS_MAX_NUM_LED_MODES],

    /* Quirk bitmask parsed from the device file's Quirks= field. */
    quirks: u32,
}

impl AsusDriver {
    pub fn new() -> Self {
        Self {
            is_ready: false,
            button_mapping: [None; ASUS_MAX_NUM_BUTTON * ASUS_MAX_NUM_BUTTON_GROUP],
            button_indices: [None; ASUS_MAX_NUM_BUTTON * ASUS_MAX_NUM_BUTTON_GROUP],
            led_modes: ASUS_DEFAULT_LED_MODES,
            quirks: 0,
        }
    }

    fn has_quirk(&self, quirk: u32) -> bool {
        self.quirks & quirk != 0
    }

    /* Initialise all per-device state from DriverConfig.
     *
     * Called once at the start of load_profiles() because DriverConfig is
     * not available at construction time.
     */
    fn init_from_config(&mut self, config: &crate::engine::device_database::DriverConfig) {
        /* 1. Quirks */
        self.quirks = parse_quirks(&config.quirks);

        /* 2. Primary button mapping: start from defaults, override with device file. */
        for i in 0..(ASUS_MAX_NUM_BUTTON * ASUS_MAX_NUM_BUTTON_GROUP) {
            self.button_mapping[i] = ASUS_DEFAULT_BUTTON_MAPPING.get(i).copied();
            self.button_indices[i] = None;
        }
        for (i, &code) in config.button_mapping.iter().enumerate().take(ASUS_MAX_NUM_BUTTON) {
            self.button_mapping[i] = Some(code);
        }

        /* 3. Secondary button group (BUTTONS_SECONDARY quirk). */
        if self.has_quirk(ASUS_QUIRK_BUTTONS_SECONDARY) {
            for (i, &code) in config
                .button_mapping_secondary
                .iter()
                .enumerate()
                .take(ASUS_MAX_NUM_BUTTON)
            {
                self.button_mapping[ASUS_MAX_NUM_BUTTON + i] = Some(code);
            }
        }

        /* 4. Build button_indices: for each entry in ASUS_BUTTON_MAPPING,
         * find the first flat position in button_mapping that holds that code. */
        let max_buttons = ASUS_MAX_NUM_BUTTON * ASUS_MAX_NUM_BUTTON_GROUP;
        let mut dev_button_idx: usize = 0;
        for entry in ASUS_BUTTON_MAPPING {
            if dev_button_idx >= max_buttons {
                break;
            }
            let flat = self.button_mapping
                .iter()
                .position(|&c| c == Some(entry.asus_code));
            if let Some(pos) = flat {
                self.button_indices[dev_button_idx] = Some(pos);
                debug!(
                    "ASUS: button {} mapped to code 0x{:02x} at flat pos {} (group {})",
                    dev_button_idx,
                    entry.asus_code,
                    pos % ASUS_MAX_NUM_BUTTON,
                    pos / ASUS_MAX_NUM_BUTTON,
                );
                dev_button_idx += 1;
            }
        }

        /* 5. LED modes: apply device-file overrides on top of the defaults. */
        self.led_modes = ASUS_DEFAULT_LED_MODES;
        for (i, mode_str) in config.led_modes.iter().enumerate().take(ASUS_MAX_NUM_LED_MODES) {
            self.led_modes[i] = parse_led_mode_str(mode_str);
        }
    }

    /* ─── Async I/O helpers ─────────────────────────────────────────────── */

    /* Send a 64-byte request and receive the 64-byte response.
     *
     * Each attempt bounds the read with a timeout so a mute device (e.g. a
     * wireless mouse that powered down mid-conversation) cannot wedge the
     * device actor forever.  Timed-out attempts resend the request — all
     * ASUS commands are idempotent — up to ASUS_QUERY_ATTEMPTS times, after
     * which the query fails with DriverError::Timeout.
     *
     * Bails with DriverError::ProtocolError if the device signals
     * ASUS_STATUS_ERROR (sleeping / disconnected wireless). */
    async fn query(&self, io: &mut DeviceIo, request: &AsusRequest) -> Result<AsusResponse> {
        let mut resp = AsusResponse::default();

        let mut attempt: u8 = 0;
        loop {
            attempt += 1;

            io.write_report(&request.buf)
                .await
                .context("ASUS: write_report failed")?;

            resp.buf = [0u8; ASUS_PACKET_SIZE];
            match tokio::time::timeout(ASUS_QUERY_TIMEOUT, io.read_report(&mut resp.buf)).await {
                Ok(Ok(_)) => break,
                Ok(Err(e)) => return Err(e).context("ASUS: read_report failed"),
                Err(_elapsed) if attempt < ASUS_QUERY_ATTEMPTS => {
                    debug!(
                        "ASUS: no response within {:?} (attempt {}/{}), retrying",
                        ASUS_QUERY_TIMEOUT, attempt, ASUS_QUERY_ATTEMPTS
                    );
                }
                Err(_elapsed) => bail!(DriverError::Timeout { attempts: attempt }),
            }
        }

        if resp.status_code() == ASUS_STATUS_ERROR {
            bail!(DriverError::ProtocolError {
                sub_id: resp.buf[0],
                error:  resp.buf[1],
            });
        }

        Ok(resp)
    }

    async fn get_profile_data(&self, io: &mut DeviceIo) -> Result<AsusProfileInfo> {
        let req = AsusRequest::new(ASUS_CMD_GET_PROFILE_DATA);
        let resp = self.query(io, &req).await?;

        /* STRIX_PROFILE: profile_id lives at results[7] instead of results[8]. */
        let profile_id = if self.has_quirk(ASUS_QUIRK_STRIX_PROFILE) {
            resp.result(7) as u32
        } else {
            resp.result(8) as u32
        };

        /* DPI preset is 1-indexed in the hardware (0 = none). */
        let dpi_preset = if resp.result(9) > 0 {
            Some(resp.result(9) as u32 - 1)
        } else {
            None
        };

        Ok(AsusProfileInfo {
            profile_id,
            dpi_preset,
            firmware_primary:   (resp.result(13), resp.result(12), resp.result(11)),
            firmware_secondary: (resp.result(4),  resp.result(3),  resp.result(2)),
        })
    }

    async fn set_profile(&self, io: &mut DeviceIo, index: u32) -> Result<()> {
        let mut req = AsusRequest::new(ASUS_CMD_SET_PROFILE);
        req.set_param(0, index as u8);
        self.query(io, &req).await?;
        Ok(())
    }

    async fn save_profile_cmd(&self, io: &mut DeviceIo) -> Result<()> {
        let req = AsusRequest::new(ASUS_CMD_SAVE);
        self.query(io, &req).await?;
        Ok(())
    }

    async fn get_binding_data(&self, io: &mut DeviceIo, group: u8) -> Result<AsusBindingData> {
        let mut req = AsusRequest::new(ASUS_CMD_GET_BUTTON_DATA);
        req.set_param(0, group);
        let resp = self.query(io, &req).await?;
        Ok(AsusBindingData::from_response(&resp))
    }

    async fn set_button_action(
        &self,
        io: &mut DeviceIo,
        asus_code_src: u8,
        asus_code_dst: u8,
        asus_type:     u8,
    ) -> Result<()> {
        let mut req = AsusRequest::new(ASUS_CMD_SET_BUTTON);
        /* params[2..5] (= buf[4..7]): source, fixed BUTTON type, destination, action type */
        req.set_param(2, asus_code_src);
        req.set_param(3, ASUS_ACTION_TYPE_BUTTON);
        req.set_param(4, asus_code_dst);
        req.set_param(5, asus_type);
        self.query(io, &req).await?;
        Ok(())
    }

    async fn get_resolution_data(
        &self,
        io:        &mut DeviceIo,
        sep_xy:    bool,
        dpi_count: usize,
    ) -> Result<AsusResolutionResult> {
        let mut req = AsusRequest::new(ASUS_CMD_GET_SETTINGS);
        req.set_param(0, if sep_xy { 2 } else { 0 });
        let resp = self.query(io, &req).await?;

        if sep_xy {
            Ok(AsusResolutionResult::Xy(AsusDpiXyData::from_response(&resp)))
        } else if dpi_count <= 2 {
            Ok(AsusResolutionResult::Dpi2(AsusDpi2Data::from_response(&resp)))
        } else {
            Ok(AsusResolutionResult::Dpi4(AsusDpi4Data::from_response(&resp)))
        }
    }

    async fn set_dpi(&self, io: &mut DeviceIo, index: u8, dpi: u32) -> Result<()> {
        let stored = dpi_to_stored(dpi, self.quirks);
        let mut req = AsusRequest::new(ASUS_CMD_SET_SETTING);
        req.set_param(0, index);   /* DPI preset slot (0-3) */
        req.set_param(2, stored);  /* stored DPI value */
        self.query(io, &req).await?;
        Ok(())
    }

    async fn set_polling_rate(&self, io: &mut DeviceIo, hz: u32, dpi_count: u8) -> Result<()> {
        let idx = polling_rate_index(hz)
            .ok_or_else(|| anyhow::anyhow!("ASUS: unsupported polling rate {} Hz", hz))?;
        let mut req = AsusRequest::new(ASUS_CMD_SET_SETTING);
        req.set_param(0, dpi_count + ASUS_FIELD_RATE);
        req.set_param(2, idx);
        self.query(io, &req).await?;
        Ok(())
    }

    async fn set_button_response(&self, io: &mut DeviceIo, ms: u32, dpi_count: u8) -> Result<()> {
        let idx = debounce_index(ms)
            .ok_or_else(|| anyhow::anyhow!("ASUS: unsupported debounce time {} ms", ms))?;
        let mut req = AsusRequest::new(ASUS_CMD_SET_SETTING);
        req.set_param(0, dpi_count + ASUS_FIELD_RESPONSE);
        req.set_param(2, idx);
        self.query(io, &req).await?;
        Ok(())
    }

    async fn set_angle_snapping(
        &self,
        io:        &mut DeviceIo,
        enabled:   bool,
        dpi_count: u8,
    ) -> Result<()> {
        let mut req = AsusRequest::new(ASUS_CMD_SET_SETTING);
        req.set_param(0, dpi_count + ASUS_FIELD_SNAPPING);
        req.set_param(2, u8::from(enabled));
        self.query(io, &req).await?;
        Ok(())
    }

    async fn get_led_data(&self, io: &mut DeviceIo, led_index: u8) -> Result<AsusLedData> {
        let mut req = AsusRequest::new(ASUS_CMD_GET_LED_DATA);
        req.set_param(0, led_index);
        let resp = self.query(io, &req).await?;
        Ok(AsusLedData::from_response(&resp))
    }

    async fn set_led(
        &self,
        io:         &mut DeviceIo,
        index:      u8,
        mode:       u8,
        brightness: u8,
        r: u8, g: u8, b: u8,
    ) -> Result<()> {
        let mut req = AsusRequest::new(ASUS_CMD_SET_LED);
        req.set_param(0, index);
        req.set_param(2, mode);
        req.set_param(3, brightness);
        req.set_param(4, r);
        req.set_param(5, g);
        req.set_param(6, b);
        self.query(io, &req).await?;
        Ok(())
    }

    /* ─── Profile loading ───────────────────────────────────────────────── */

    async fn load_single_profile(
        &self,
        io:         &mut DeviceIo,
        profile:    &mut ProfileInfo,
        dpi_preset: Option<u32>,
    ) -> Result<()> {
        let dpi_count = profile.resolutions.len();
        let led_count = profile.leds.len();

        /* ── Buttons ─────────────────────────────────────────────────────── */
        debug!("ASUS: loading buttons for profile {}", profile.index);
        let binding = self.get_binding_data(io, 0).await?;

        let binding_secondary =
            if self.has_quirk(ASUS_QUIRK_BUTTONS_SECONDARY) {
                Some(self.get_binding_data(io, 1).await?)
            } else {
                None
            };

        for btn in &mut profile.buttons {
            let dev_idx = btn.index as usize;
            if dev_idx >= ASUS_MAX_NUM_BUTTON * ASUS_MAX_NUM_BUTTON_GROUP {
                continue;
            }
            let Some(flat) = self.button_indices[dev_idx] else {
                debug!("ASUS: no mapping for DeviceInfo button {}", dev_idx);
                continue;
            };

            let wire = if flat < ASUS_MAX_NUM_BUTTON {
                binding.bindings.get(flat).copied()
            } else {
                binding_secondary
                    .as_ref()
                    .and_then(|b| b.bindings.get(flat % ASUS_MAX_NUM_BUTTON))
                    .copied()
            };

            let Some(wire) = wire else {
                continue;
            };

            if wire.action == ASUS_BUTTON_CODE_DISABLED {
                btn.action_type = ActionType::None;
                continue;
            }

            match wire.type_ {
                ASUS_ACTION_TYPE_KEY => {
                    if let Some(linux_code) = get_linux_key_code(wire.action) {
                        btn.action_type  = ActionType::Key;
                        btn.mapping_value = linux_code;
                    } else {
                        debug!("ASUS: unknown key code 0x{:02x}", wire.action);
                    }
                }
                ASUS_ACTION_TYPE_BUTTON => {
                    if let Some(entry) = find_button_by_code(wire.action) {
                        match entry.kind {
                            AsusButtonKind::Button(n) => {
                                btn.action_type  = ActionType::Button;
                                btn.mapping_value = n;
                            }
                            AsusButtonKind::Special(n) => {
                                btn.action_type  = ActionType::Special;
                                btn.mapping_value = n;
                            }
                            AsusButtonKind::Joystick => {
                                /* Joystick axes reported as Special with value 0
                                 * until a more precise action can be assigned. */
                                btn.action_type  = ActionType::Special;
                                btn.mapping_value = 0;
                            }
                            AsusButtonKind::None => {
                                btn.action_type = ActionType::None;
                            }
                        }
                    } else {
                        debug!("ASUS: unknown action code 0x{:02x}", wire.action);
                    }
                }
                other => {
                    debug!("ASUS: unknown button type 0x{:02x}", other);
                }
            }
        }

        /* ── DPI / settings ─────────────────────────────────────────────── */
        debug!("ASUS: loading resolutions for profile {}", profile.index);
        let res_data = self.get_resolution_data(io, false, dpi_count).await?;

        let xy_data =
            if self.has_quirk(ASUS_QUIRK_SEPARATE_XY_DPI) {
                match self.get_resolution_data(io, true, dpi_count).await? {
                    AsusResolutionResult::Xy(d) => Some(d),
                    _ => bail!("ASUS: expected XY response for separate XY DPI query"),
                }
            } else {
                None
            };

        /* Destructure the variant-specific data into a common shape so we
         * can handle Dpi2 and Dpi4 with a single code path. */
        let (dpis, rate_idx, response_idx, snapping): (&[u16], u16, u16, u16) = match res_data {
            AsusResolutionResult::Dpi2(ref d) => (&d.dpi, d.rate_idx, d.response_idx, d.snapping),
            AsusResolutionResult::Dpi4(ref d) => (&d.dpi, d.rate_idx, d.response_idx, d.snapping),
            AsusResolutionResult::Xy(_) => {
                bail!("ASUS: unexpected XY response for non-XY DPI query");
            }
        };

        profile.report_rate = ASUS_POLLING_RATES
            .get(rate_idx as usize)
            .copied()
            .unwrap_or(1000);
        if response_idx < ASUS_DEBOUNCE_TIMES.len() as u16 {
            profile.debounce = ASUS_DEBOUNCE_TIMES[response_idx as usize] as i32;
        }
        profile.angle_snapping = snapping as i32;
        for res in &mut profile.resolutions {
            let i = res.index as usize;
            res.dpi = self.build_dpi(
                i, dpis.get(i).copied().unwrap_or(0), xy_data.as_ref(),
            );
            if let Some(preset) = dpi_preset {
                res.is_active = res.index == preset;
            }
        }

        /* ── LEDs ────────────────────────────────────────────────────────── */
        if led_count == 0 {
            return Ok(());
        }

        debug!("ASUS: loading LEDs for profile {}", profile.index);

        /* Fetch all LEDs in one query unless the device requires separate queries. */
        let bulk = if !self.has_quirk(ASUS_QUIRK_SEPARATE_LEDS) {
            Some(self.get_led_data(io, 0).await?)
        } else {
            None
        };

        for led in &mut profile.leds {
            let entry: AsusLedEntry = if self.has_quirk(ASUS_QUIRK_SEPARATE_LEDS) {
                let data = self.get_led_data(io, led.index as u8).await?;
                /* SEPARATE_LEDS: response carries the single LED in leds[0]. */
                data.leds[0]
            } else {
                bulk.as_ref()
                    .and_then(|d| d.leds.get(led.index as usize))
                    .copied()
                    .unwrap_or_default()
            };

            let mode_idx = entry.mode as usize;
            led.mode = self.led_modes.get(mode_idx).copied().unwrap_or(LedMode::Solid);
            led.brightness = brightness_to_ratbag(entry.brightness, self.quirks);
            led.color = Color {
                red:   entry.r as u32,
                green: entry.g as u32,
                blue:  entry.b as u32,
            };
        }

        Ok(())
    }

    /* Build a Dpi value for resolution slot `i`, combining unified DPI with
     * optional separate-XY data. */
    fn build_dpi(&self, i: usize, stored: u16, xy_data: Option<&AsusDpiXyData>) -> Dpi {
        if let Some(xy) = xy_data {
            if let Some(&(xs, ys)) = xy.dpi.get(i) {
                return Dpi::Separate {
                    x: dpi_from_stored(xs, self.quirks),
                    y: dpi_from_stored(ys, self.quirks),
                };
            }
        }
        Dpi::Unified(dpi_from_stored(stored, self.quirks))
    }

    async fn load_all_profiles(&self, io: &mut DeviceIo, info: &mut DeviceInfo) -> Result<()> {
        let pinfo = self.get_profile_data(io).await?;
        let initial_id = if info.profiles.len() > 1 {
            pinfo.profile_id
        } else {
            0
        };

        debug!(
            "ASUS: firmware primary {:02X}.{:02X}.{:02X}, secondary {:02X}.{:02X}.{:02X}",
            pinfo.firmware_primary.0,   pinfo.firmware_primary.1,   pinfo.firmware_primary.2,
            pinfo.firmware_secondary.0, pinfo.firmware_secondary.1, pinfo.firmware_secondary.2,
        );
        info.firmware_version = format!(
            "{:02X}.{:02X}.{:02X}",
            pinfo.firmware_primary.0, pinfo.firmware_primary.1, pinfo.firmware_primary.2
        );

        let num_profiles = info.profiles.len();
        for i in 0..num_profiles {
            let current_id = info.profiles[i].index;

            if current_id != initial_id {
                info.profiles[i].is_active = false;
                debug!("ASUS: switching to profile {}", current_id);
                self.set_profile(io, current_id).await?;
            } else {
                info.profiles[i].is_active = true;
            }

            let dpi_preset = pinfo.dpi_preset;
            self.load_single_profile(io, &mut info.profiles[i], dpi_preset)
                .await?;
        }

        /* Restore the originally active profile. */
        if num_profiles > 1 {
            debug!("ASUS: restoring profile {}", initial_id);
            self.set_profile(io, initial_id).await?;
        }

        Ok(())
    }

    /* ─── Profile saving ────────────────────────────────────────────────── */

    async fn save_single_profile(
        &self,
        io:      &mut DeviceIo,
        profile: &ProfileInfo,
    ) -> Result<()> {
        let dpi_count = profile.resolutions.len() as u8;

        /* ── Buttons ─────────────────────────────────────────────────────── */
        for btn in &profile.buttons {
            let dev_idx = btn.index as usize;
            if dev_idx >= ASUS_MAX_NUM_BUTTON * ASUS_MAX_NUM_BUTTON_GROUP {
                continue;
            }

            let Some(flat) = self.button_indices[dev_idx] else {
                debug!("ASUS: no mapping for button {}", dev_idx);
                continue;
            };

            let Some(src_code) = self.button_mapping.get(flat).copied().flatten() else {
                continue;
            };

            match btn.action_type {
                ActionType::None => {
                    self.set_button_action(
                        io, src_code, ASUS_BUTTON_CODE_DISABLED, ASUS_ACTION_TYPE_BUTTON,
                    )
                    .await?;
                }
                ActionType::Key => {
                    let Some(asus_key) = find_key_code(btn.mapping_value) else {
                        debug!("ASUS: no key code for Linux code {}", btn.mapping_value);
                        continue;
                    };
                    self.set_button_action(io, src_code, asus_key, ASUS_ACTION_TYPE_KEY)
                        .await?;
                }
                ActionType::Button | ActionType::Special => {
                    let is_joy = is_joystick_code(src_code);
                    let entry = if is_joy {
                        find_button_by_action(btn.action_type, btn.mapping_value, true)
                            .or_else(|| {
                                find_button_by_action(btn.action_type, btn.mapping_value, false)
                            })
                    } else {
                        find_button_by_action(btn.action_type, btn.mapping_value, false)
                    };
                    let Some(e) = entry else {
                        debug!(
                            "ASUS: no ASUS code for action {:?} value {}",
                            btn.action_type, btn.mapping_value
                        );
                        continue;
                    };
                    self.set_button_action(
                        io, src_code, e.asus_code, ASUS_ACTION_TYPE_BUTTON,
                    )
                    .await?;
                }
                other => {
                    /* The ASUS protocol has no macro (or other) bindings —
                     * leave the button's on-device binding untouched. */
                    warn!(
                        "ASUS: unsupported action type {:?} for button {}, skipping",
                        other, btn.index
                    );
                    continue;
                }
            }
        }

        /* ── Polling rate, angle snapping, debounce ─────────────────────── */
        if profile.report_rate > 0 {
            self.set_polling_rate(io, profile.report_rate, dpi_count).await?;
        }
        if profile.angle_snapping >= 0 {
            self.set_angle_snapping(io, profile.angle_snapping != 0, dpi_count).await?;
        }
        if profile.debounce > 0 {
            self.set_button_response(io, profile.debounce as u32, dpi_count).await?;
        }

        /* ── DPI presets ─────────────────────────────────────────────────── */
        let mut clamp_warned = false;
        for res in &profile.resolutions {
            let dpi_val = match res.dpi {
                Dpi::Unified(v) => v,
                /* For separate-XY devices the protocol only accepts a single value;
                 * use X (matches C driver behaviour). */
                Dpi::Separate { x, .. } => x,
                Dpi::Unknown => continue,
            };
            if dpi_val > max_wire_dpi(self.quirks) && !clamp_warned {
                warn!(
                    "ASUS: requested DPI {} exceeds the wire limit of {}, clamping",
                    dpi_val,
                    max_wire_dpi(self.quirks)
                );
                clamp_warned = true;
            }
            self.set_dpi(io, res.index as u8, dpi_val).await?;
        }

        /* ── LEDs ────────────────────────────────────────────────────────── */
        for led in &profile.leds {
            /* Find ASUS mode index by scanning led_modes for a match. */
            let asus_mode = self
                .led_modes
                .iter()
                .position(|&m| m == led.mode)
                .unwrap_or(0) as u8;

            let asus_brightness = brightness_to_asus(led.brightness, self.quirks);
            let rgb = led.color.to_rgb();

            self.set_led(
                io,
                led.index as u8,
                asus_mode,
                asus_brightness,
                rgb.r, rgb.g, rgb.b,
            )
            .await?;
        }

        Ok(())
    }

    async fn save_all_profiles(&self, io: &mut DeviceIo, info: &DeviceInfo) -> Result<()> {
        let num_profiles = info.profiles.len();
        if num_profiles == 0 {
            return Ok(());
        }

        let initial_id = if num_profiles > 1 {
            self.get_profile_data(io).await?.profile_id
        } else {
            0
        };

        /* Track which profile the hardware is actually on: each dirty
         * profile must be written while it is the selected one. */
        let mut current_id = initial_id;

        for profile in &info.profiles {
            if !profile.is_dirty {
                continue;
            }

            debug!("ASUS: saving profile {}", profile.index);

            if num_profiles > 1 && profile.index != current_id {
                self.set_profile(io, profile.index).await?;
                current_id = profile.index;
            }

            self.save_single_profile(io, profile).await?;

            debug!("ASUS: persisting profile {}", profile.index);
            self.save_profile_cmd(io).await?;
        }

        /* Leave the hardware on the profile marked active in DeviceInfo —
         * this is how DBus SetActive reaches the device (the engine folds
         * active-profile switches into commit).  Fall back to the profile
         * that was active on the hardware when the commit started. */
        let target_id = info
            .profiles
            .iter()
            .find(|p| p.is_active)
            .map_or(initial_id, |p| p.index);
        if num_profiles > 1 && target_id != current_id {
            debug!("ASUS: switching to active profile {}", target_id);
            self.set_profile(io, target_id).await?;
        }

        Ok(())
    }
}

/* Tagged union returned by get_resolution_data(). */
enum AsusResolutionResult {
    Dpi2(AsusDpi2Data),
    Dpi4(AsusDpi4Data),
    Xy(AsusDpiXyData),
}

// ─────────────────────── DeviceDriver impl ─────────────────────────────────

#[async_trait]
impl DeviceDriver for AsusDriver {
    fn name(&self) -> &str {
        "asus"
    }

    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()> {
        /* A successful GET_PROFILE_DATA confirms the device is reachable. */
        let req = AsusRequest::new(ASUS_CMD_GET_PROFILE_DATA);
        match self.query(io, &req).await {
            Ok(_) => {
                self.is_ready = true;
                debug!("ASUS: probe succeeded");
                Ok(())
            }
            Err(e) if is_asleep_error(&e) => {
                /* Sleeping/disconnected wireless mouse.  Register the device
                 * on DBus anyway; commit() retries when it wakes up. */
                warn!("ASUS: device is sleeping or disconnected: {}", e);
                self.is_ready = false;
                Ok(())
            }
            Err(e) => {
                /* Hard transport error — do not register a dead node. */
                Err(e.context("ASUS: probe failed"))
            }
        }
    }

    async fn load_profiles(&mut self, io: &mut DeviceIo, info: &mut DeviceInfo) -> Result<()> {
        /* Initialise all driver-side state from the device-file config. */
        self.init_from_config(&info.driver_config);

        /* Fill static per-profile capability lists that don't need hardware I/O. */
        let led_modes_vec: Vec<LedMode> = self.led_modes.to_vec();
        for profile in &mut info.profiles {
            profile.report_rates = ASUS_POLLING_RATES.to_vec();
            profile.debounces    = ASUS_DEBOUNCE_TIMES.to_vec();
            for led in &mut profile.leds {
                led.color_depth = 1; /* RGB 888, matching the other RGB drivers */
                led.modes = led_modes_vec.clone();
            }
        }

        match self.load_all_profiles(io, info).await {
            Ok(()) => {
                self.is_ready = true;
                Ok(())
            }
            Err(e) if is_asleep_error(&e) => {
                warn!("ASUS: device asleep, exposing skeleton profiles: {}", e);
                self.is_ready = false;
                /* The skeleton DeviceInfo is still valid for DBus exposure,
                 * but every consumer expects exactly one active profile. */
                if !info.profiles.iter().any(|p| p.is_active)
                    && let Some(first) = info.profiles.first_mut()
                {
                    first.is_active = true;
                }
                Ok(())
            }
            Err(e) => Err(e.context("ASUS: loading profiles failed")),
        }
    }

    async fn commit(&mut self, io: &mut DeviceIo, info: &DeviceInfo) -> Result<()> {
        if !self.is_ready {
            /* Device was sleeping at probe time — attempt recovery using a
             * scratch clone of info (we do not want to modify info here). */
            warn!("ASUS: device was not ready, attempting reload before commit");
            let mut scratch = info.clone();
            match self.load_all_profiles(io, &mut scratch).await {
                Ok(()) => {
                    self.is_ready = true;
                    debug!("ASUS: device recovery succeeded");
                }
                Err(e) => {
                    warn!("ASUS: device recovery failed: {}", e);
                    bail!(
                        "ASUS: device is sleeping or disconnected and did not \
                         wake up — commit aborted"
                    );
                }
            }
            /* Even after successful recovery, abort this commit as the C driver
             * does: we rolled back instead of committing. */
            bail!(
                "ASUS: device was asleep when its state was read; it has been \
                 reloaded — re-apply the changes and commit again"
            );
        }

        match self.save_all_profiles(io, info).await {
            Ok(()) => Ok(()),
            Err(e) => {
                /* If the device fell asleep mid-commit, remember it so the
                 * next commit goes through the recovery path first. */
                if is_asleep_error(&e) {
                    self.is_ready = false;
                }
                Err(e)
            }
        }
    }
}

// ──────────────────────────── Unit tests ───────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /* ── DPI conversion ──────────────────────────────────────────────────── */

    #[test]
    fn test_dpi_from_stored_basic() {
        assert_eq!(dpi_from_stored(0, 0), 50);
        assert_eq!(dpi_from_stored(1, 0), 100);
        assert_eq!(dpi_from_stored(19, 0), 1000);   /* (19*50)+50 = 1000 */
        assert_eq!(dpi_from_stored(239, 0), 12000);  /* (239*50)+50 = 12000 */
    }

    #[test]
    fn test_dpi_from_stored_double_dpi() {
        assert_eq!(dpi_from_stored(19, ASUS_QUIRK_DOUBLE_DPI), 2000);
        assert_eq!(dpi_from_stored(0,  ASUS_QUIRK_DOUBLE_DPI), 100);
    }

    #[test]
    fn test_dpi_roundtrip() {
        for &dpi in &[100u32, 400, 800, 1600, 3200, 6400, 12000] {
            let stored = dpi_to_stored(dpi, 0);
            assert_eq!(dpi_from_stored(stored as u16, 0), dpi,
                "roundtrip failed for {} DPI", dpi);
        }
    }

    #[test]
    fn test_dpi_roundtrip_double_dpi() {
        for &dpi in &[200u32, 800, 1600, 3200, 6400, 12800] {
            let stored = dpi_to_stored(dpi, ASUS_QUIRK_DOUBLE_DPI);
            assert_eq!(dpi_from_stored(stored as u16, ASUS_QUIRK_DOUBLE_DPI), dpi,
                "DOUBLE_DPI roundtrip failed for {} DPI", dpi);
        }
    }

    /* ── Button table lookups ────────────────────────────────────────────── */

    #[test]
    fn test_find_button_by_code_left() {
        let e = find_button_by_code(0xf0).expect("left button must be in table");
        assert_eq!(e.kind, AsusButtonKind::Button(1));
    }

    #[test]
    fn test_find_button_by_code_missing() {
        assert!(find_button_by_code(0x00).is_none());
    }

    #[test]
    fn test_find_button_by_code_wheel_up() {
        let e = find_button_by_code(0xe8).expect("wheel-up must be in table");
        assert_eq!(e.kind, AsusButtonKind::Special(SPECIAL_WHEEL_UP));
    }

    #[test]
    fn test_find_button_by_action_button() {
        let e = find_button_by_action(ActionType::Button, 1, false)
            .expect("left click action must be found");
        assert_eq!(e.asus_code, 0xf0);
    }

    #[test]
    fn test_find_button_by_action_special() {
        let e = find_button_by_action(ActionType::Special, SPECIAL_WHEEL_UP, false)
            .expect("wheel-up special must be found");
        assert_eq!(e.asus_code, 0xe8);
    }

    #[test]
    fn test_find_button_by_action_joystick() {
        /* Joystick wheel-up should use joystick axis code (0xd8). */
        let e = find_button_by_action(ActionType::Special, SPECIAL_WHEEL_UP, true)
            .expect("joystick wheel-up must be found");
        assert_eq!(e.asus_code, 0xd8);
    }

    /* ── Key code table ──────────────────────────────────────────────────── */

    #[test]
    fn test_get_linux_key_code_a() {
        assert_eq!(get_linux_key_code(0x04), Some(KEY_A));
    }

    #[test]
    fn test_get_linux_key_code_unmapped() {
        assert!(get_linux_key_code(0x00).is_none());
    }

    #[test]
    fn test_find_key_code_roundtrip() {
        let asus = find_key_code(KEY_A).expect("KEY_A should have an ASUS code");
        assert_eq!(get_linux_key_code(asus), Some(KEY_A));
    }

    /* ── Misc helpers ────────────────────────────────────────────────────── */

    #[test]
    fn test_is_joystick_code() {
        assert!(is_joystick_code(0xd0));
        assert!(is_joystick_code(0xdb));
        assert!(!is_joystick_code(0xf0));
        assert!(!is_joystick_code(0x00));
    }

    #[test]
    fn test_polling_rate_index() {
        assert_eq!(polling_rate_index(125),  Some(0));
        assert_eq!(polling_rate_index(250),  Some(1));
        assert_eq!(polling_rate_index(500),  Some(2));
        assert_eq!(polling_rate_index(1000), Some(3));
        assert_eq!(polling_rate_index(333),  None);
    }

    #[test]
    fn test_debounce_index() {
        assert_eq!(debounce_index(4),  Some(0));
        assert_eq!(debounce_index(32), Some(7));
        assert_eq!(debounce_index(5),  None);
    }

    /* ── Brightness ──────────────────────────────────────────────────────── */

    #[test]
    fn test_brightness_to_ratbag_normal() {
        assert_eq!(brightness_to_ratbag(0, 0), 0);
        assert_eq!(brightness_to_ratbag(4, 0), 256);
    }

    #[test]
    fn test_brightness_to_ratbag_raw() {
        assert_eq!(brightness_to_ratbag(200, ASUS_QUIRK_RAW_BRIGHTNESS), 200);
    }

    #[test]
    fn test_brightness_roundtrip() {
        for &b in &[0u32, 64, 128, 192] {
            let asus = brightness_to_asus(b, 0);
            let back = brightness_to_ratbag(asus, 0);
            assert!(
                (back as i64 - b as i64).abs() <= 32,
                "brightness {} → hw {} → {} (diff > 32)", b, asus, back
            );
        }
    }

    /* ── Quirk parsing ───────────────────────────────────────────────────── */

    #[test]
    fn test_parse_quirks_combined() {
        let q = parse_quirks(&[
            "DOUBLE_DPI".to_string(),
            "STRIX_PROFILE".to_string(),
            "SEPARATE_LEDS".to_string(),
        ]);
        assert!(q & ASUS_QUIRK_DOUBLE_DPI    != 0);
        assert!(q & ASUS_QUIRK_STRIX_PROFILE  != 0);
        assert!(q & ASUS_QUIRK_SEPARATE_LEDS  != 0);
        assert!(q & ASUS_QUIRK_BATTERY_V2     == 0);
    }

    #[test]
    fn test_parse_quirks_empty() {
        assert_eq!(parse_quirks(&[]), 0);
    }

    /* ── Packet construction ─────────────────────────────────────────────── */

    #[test]
    fn test_asus_request_cmd_le_encoding() {
        /* GET_PROFILE_DATA = 0x0012 → LE bytes [0x12, 0x00] */
        let req = AsusRequest::new(ASUS_CMD_GET_PROFILE_DATA);
        assert_eq!(req.buf[0], 0x12);
        assert_eq!(req.buf[1], 0x00);
    }

    #[test]
    fn test_asus_request_set_param() {
        let mut req = AsusRequest::new(ASUS_CMD_SET_PROFILE);
        req.set_param(0, 2u8);
        assert_eq!(req.buf[2], 2); /* params[0] = buf[2] */
    }

    #[test]
    fn test_asus_response_status_error() {
        let mut resp = AsusResponse::default();
        resp.buf[0] = 0xff;
        resp.buf[1] = 0xaa;
        assert_eq!(resp.status_code(), ASUS_STATUS_ERROR);
    }

    #[test]
    fn test_asus_response_result_offset() {
        let mut resp = AsusResponse::default();
        resp.buf[2] = 0xab; /* result(0) */
        resp.buf[9] = 0xcd; /* result(7) */
        assert_eq!(resp.result(0), 0xab);
        assert_eq!(resp.result(7), 0xcd);
    }

    /* ── Binding data parsing ────────────────────────────────────────────── */

    #[test]
    fn test_binding_data_from_response() {
        let mut resp = AsusResponse::default();
        /* binding[0]: action=0xf0, type=BUTTON at result(2),result(3) = buf[4],buf[5] */
        resp.buf[4] = 0xf0;
        resp.buf[5] = ASUS_ACTION_TYPE_BUTTON;
        /* binding[1]: action=0xe8, type=BUTTON at result(4),result(5) = buf[6],buf[7] */
        resp.buf[6] = 0xe8;
        resp.buf[7] = ASUS_ACTION_TYPE_BUTTON;

        let data = AsusBindingData::from_response(&resp);
        assert_eq!(data.bindings[0].action, 0xf0);
        assert_eq!(data.bindings[0].type_,  ASUS_ACTION_TYPE_BUTTON);
        assert_eq!(data.bindings[1].action, 0xe8);
    }

    /* ── LED data parsing ────────────────────────────────────────────────── */

    #[test]
    fn test_led_data_from_response() {
        let mut resp = AsusResponse::default();
        /* led[0]: mode=1, brightness=2, r=10, g=20, b=30
         * At result(2..6) = buf[4..8] */
        resp.buf[4] = 1;
        resp.buf[5] = 2;
        resp.buf[6] = 10;
        resp.buf[7] = 20;
        resp.buf[8] = 30;

        let data = AsusLedData::from_response(&resp);
        let led = &data.leds[0];
        assert_eq!(led.mode, 1);
        assert_eq!(led.brightness, 2);
        assert_eq!(led.r, 10);
        assert_eq!(led.g, 20);
        assert_eq!(led.b, 30);
    }

    /* ── DPI data parsing ────────────────────────────────────────────────── */

    #[test]
    fn test_dpi4_from_response() {
        let mut resp = AsusResponse::default();
        /* dpi[0] = 0x000F (stored 15) at result(2..3) = buf[4..5] */
        resp.buf[4] = 0x0f;
        resp.buf[5] = 0x00;
        /* rate = 3 at result(10..11) = buf[12..13] */
        resp.buf[12] = 3;
        resp.buf[13] = 0;

        let data = AsusDpi4Data::from_response(&resp);
        assert_eq!(data.dpi[0], 15);
        assert_eq!(data.rate_idx, 3);
        assert_eq!(dpi_from_stored(data.dpi[0], 0), 800); /* 15*50+50 = 800 */
        assert_eq!(ASUS_POLLING_RATES[data.rate_idx as usize], 1000);
    }

    /* ── Protocol-level tests against the mock HID transport ─────────────── */

    use crate::engine::device::ActionType as At;
    use crate::engine::device_database::{DeviceEntry, DriverConfig};
    use crate::hal::mock::{MockExchange, MockReply};

    /* Expected request bytes: command word + (param index, value) pairs. */
    fn req_bytes(cmd: u16, params: &[(usize, u8)]) -> Vec<u8> {
        let mut r = AsusRequest::new(cmd);
        for &(i, v) in params {
            r.set_param(i, v);
        }
        r.buf.to_vec()
    }

    /* Successful response: zero status + (result index, value) pairs. */
    fn ok_resp(results: &[(usize, u8)]) -> Vec<u8> {
        let mut b = vec![0u8; ASUS_PACKET_SIZE];
        for &(i, v) in results {
            b[2 + i] = v;
        }
        b
    }

    /* Response carrying ASUS_STATUS_ERROR (device sleeping/disconnected). */
    fn asleep_resp() -> Vec<u8> {
        let mut b = vec![0u8; ASUS_PACKET_SIZE];
        b[0..2].copy_from_slice(&ASUS_STATUS_ERROR.to_le_bytes());
        b
    }

    fn silence() -> MockExchange {
        MockExchange { expect: None, reply: MockReply::Silence }
    }

    /* Build a DeviceInfo the same way the daemon does: through a synthetic
     * DeviceEntry, so vector sizing and defaults match production. */
    #[allow(clippy::too_many_arguments)]
    fn make_info(
        profiles: u32,
        buttons: u32,
        leds: u32,
        dpis: u32,
        quirks: &[&str],
        mapping: &[u8],
        mapping_secondary: &[u8],
    ) -> DeviceInfo {
        let entry = DeviceEntry {
            name: "Test Mouse".into(),
            driver: "asus".into(),
            device_type: "mouse".into(),
            matches: Vec::new(),
            driver_config: Some(DriverConfig {
                profiles: Some(profiles),
                buttons: Some(buttons),
                leds: Some(leds),
                dpis: Some(dpis),
                quirks: quirks.iter().map(|s| s.to_string()).collect(),
                button_mapping: mapping.to_vec(),
                button_mapping_secondary: mapping_secondary.to_vec(),
                ..DriverConfig::default()
            }),
        };
        DeviceInfo::from_entry("test0", "Test Mouse", 0x03, 0x0b05, 0x1877, &entry)
    }

    #[tokio::test]
    async fn probe_succeeds_and_sets_ready() {
        let (mut io, handle) = DeviceIo::with_mock(vec![MockExchange::expect_reply(
            req_bytes(ASUS_CMD_GET_PROFILE_DATA, &[]),
            ok_resp(&[]),
        )]);
        let mut drv = AsusDriver::new();

        drv.probe(&mut io).await.expect("probe should succeed");

        assert!(drv.is_ready);
        assert!(handle.script_exhausted());
    }

    #[tokio::test]
    async fn probe_tolerates_sleeping_device() {
        let (mut io, _handle) =
            DeviceIo::with_mock(vec![MockExchange::reply(asleep_resp())]);
        let mut drv = AsusDriver::new();

        drv.probe(&mut io)
            .await
            .expect("sleeping device must still be registered");

        assert!(!drv.is_ready);
    }

    /* start_paused: tokio auto-advances the clock while every task is idle,
     * so the three 500 ms read deadlines elapse instantly. */
    #[tokio::test(start_paused = true)]
    async fn probe_treats_mute_device_as_asleep() {
        let (mut io, handle) =
            DeviceIo::with_mock(vec![silence(), silence(), silence()]);
        let mut drv = AsusDriver::new();

        drv.probe(&mut io)
            .await
            .expect("mute device must still be registered");

        assert!(!drv.is_ready);
        /* One write per timed-out attempt. */
        assert_eq!(handle.writes().len(), ASUS_QUERY_ATTEMPTS as usize);
    }

    #[tokio::test]
    async fn probe_fails_on_hard_transport_error() {
        let (mut io, _handle) = DeviceIo::with_mock(vec![MockExchange {
            expect: None,
            reply: MockReply::WriteError,
        }]);
        let mut drv = AsusDriver::new();

        assert!(drv.probe(&mut io).await.is_err());
        assert!(!drv.is_ready);
    }

    #[tokio::test(start_paused = true)]
    async fn query_retries_after_timeout() {
        let (mut io, handle) =
            DeviceIo::with_mock(vec![silence(), MockExchange::reply(ok_resp(&[]))]);
        let mut drv = AsusDriver::new();

        drv.probe(&mut io)
            .await
            .expect("second attempt should succeed");

        assert!(drv.is_ready);
        assert_eq!(handle.writes().len(), 2);
    }

    /* Full profile load for a Gladius-II-Origin-like device:
     * 3 buttons of interest, 2 DPI presets, 3 LEDs, DOUBLE_DPI quirk. */
    #[tokio::test]
    async fn load_profiles_reads_full_state() {
        let mut info = make_info(2, 8, 3, 2, &["DOUBLE_DPI"], &[], &[]);

        /* Hardware sits on profile 1; DPI preset 2 (1-indexed) = slot 1. */
        let profile_data = ok_resp(&[
            (8, 1),           /* active profile id */
            (9, 2),           /* dpi preset (1-indexed) */
            (13, 0x01), (12, 0x02), (11, 0x03), /* primary fw 01.02.03 */
        ]);
        let buttons = ok_resp(&[
            /* binding[0] = (0xf0, BUTTON)  -> left button */
            (2, 0xf0), (3, ASUS_ACTION_TYPE_BUTTON),
            /* binding[1] = (0x04, KEY)     -> key A */
            (4, 0x04), (5, ASUS_ACTION_TYPE_KEY),
            /* binding[2] = disabled */
            (6, ASUS_BUTTON_CODE_DISABLED),
            /* binding[6] = (0xe8, BUTTON)  -> wheel up special */
            (14, 0xe8), (15, ASUS_ACTION_TYPE_BUTTON),
        ]);
        let settings = ok_resp(&[
            (2, 15),  /* dpi[0] stored 15 -> (15*50+50)*2 = 1600 */
            (4, 31),  /* dpi[1] stored 31 -> 3200 */
            (6, 2),   /* rate index 2 -> 500 Hz */
            (8, 1),   /* debounce index 1 -> 8 ms */
            (10, 1),  /* angle snapping on */
        ]);
        let leds = ok_resp(&[
            (2, 1), (3, 2), (4, 0x11), (5, 0x22), (6, 0x33), /* led0: breathing, 2 -> 128 */
            (7, 0), (8, 4), (9, 0xff),                       /* led1: solid, 4 -> 256, red */
        ]);

        let (mut io, handle) = DeviceIo::with_mock(vec![
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_PROFILE_DATA, &[]), profile_data),
            /* profile 0 is not the active one: switch, read, ... */
            MockExchange::expect_reply(req_bytes(ASUS_CMD_SET_PROFILE, &[(0, 0)]), ok_resp(&[])),
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_BUTTON_DATA, &[(0, 0)]), buttons.clone()),
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_SETTINGS, &[(0, 0)]), settings.clone()),
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_LED_DATA, &[(0, 0)]), leds.clone()),
            /* ... profile 1 is already selected: read without switching ... */
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_BUTTON_DATA, &[(0, 0)]), buttons),
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_SETTINGS, &[(0, 0)]), settings),
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_LED_DATA, &[(0, 0)]), leds),
            /* ... and restore the originally active profile. */
            MockExchange::expect_reply(req_bytes(ASUS_CMD_SET_PROFILE, &[(0, 1)]), ok_resp(&[])),
        ]);

        let mut drv = AsusDriver::new();
        drv.load_profiles(&mut io, &mut info).await.unwrap();

        assert!(drv.is_ready);
        assert!(handle.script_exhausted());
        assert_eq!(info.firmware_version, "01.02.03");
        assert!(!info.profiles[0].is_active);
        assert!(info.profiles[1].is_active);

        let p = &info.profiles[0];
        assert_eq!(p.buttons[0].action_type, At::Button);
        assert_eq!(p.buttons[0].mapping_value, 1);
        assert_eq!(p.buttons[1].action_type, At::Key);
        assert_eq!(p.buttons[1].mapping_value, KEY_A);
        assert_eq!(p.buttons[2].action_type, At::None);
        /* DeviceInfo button 3 reads flat slot 6 (default map: wheel up). */
        assert_eq!(p.buttons[3].action_type, At::Special);
        assert_eq!(p.buttons[3].mapping_value, SPECIAL_WHEEL_UP);

        assert!(matches!(p.resolutions[0].dpi, Dpi::Unified(1600)));
        assert!(matches!(p.resolutions[1].dpi, Dpi::Unified(3200)));
        assert!(!p.resolutions[0].is_active);
        assert!(p.resolutions[1].is_active); /* from dpi preset */
        assert_eq!(p.report_rate, 500);
        assert_eq!(p.debounce, 8);
        assert_eq!(p.angle_snapping, 1);

        assert_eq!(p.leds[0].mode, LedMode::Breathing);
        assert_eq!(p.leds[0].brightness, 128);
        assert_eq!(p.leds[0].color, Color { red: 0x11, green: 0x22, blue: 0x33 });
        assert_eq!(p.leds[1].mode, LedMode::Solid);
        assert_eq!(p.leds[1].brightness, 256);
        assert_eq!(p.leds[0].color_depth, 1);
    }

    /* Chakram-X-like device: every quirk that changes the wire conversation
     * at once (DOUBLE_DPI, RAW_BRIGHTNESS, SEPARATE_XY_DPI, SEPARATE_LEDS,
     * BUTTONS_SECONDARY) plus custom button mappings with gaps. */
    #[tokio::test]
    async fn load_profiles_chakram_x_quirk_combo() {
        let mut info = make_info(
            1, 14, 3, 4,
            &["DOUBLE_DPI", "RAW_BRIGHTNESS", "SEPARATE_XY_DPI", "SEPARATE_LEDS", "BUTTONS_SECONDARY"],
            &[0xf0, 0xf1, 0xf2, 0, 0, 0xe6, 0xe8, 0xe9, 0xd0, 0xd1, 0xd2, 0xd3],
            &[0, 0, 0xea, 0xeb, 0xec, 0xed],
        );

        /* binding[8] (joystick up, flat slot 8) bound to itself. */
        let buttons_primary = ok_resp(&[(18, 0xd0), (19, ASUS_ACTION_TYPE_BUTTON)]);
        /* secondary binding[2] (side button A, flat slot 19) = key SPACE. */
        let buttons_secondary = ok_resp(&[(6, 0x2c), (7, ASUS_ACTION_TYPE_KEY)]);
        let settings = ok_resp(&[(10, 3)]); /* rate index 3 -> 1000 Hz */
        let settings_xy = ok_resp(&[
            (2, 99),  /* xy[0].x stored -> (99*50+50)*2  = 10000 */
            (4, 199), /* xy[0].y stored -> (199*50+50)*2 = 20000 */
            (6, 44), (8, 54), /* xy[1] = 4500 / 5500 */
        ]);

        let (mut io, handle) = DeviceIo::with_mock(vec![
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_PROFILE_DATA, &[]), ok_resp(&[])),
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_BUTTON_DATA, &[(0, 0)]), buttons_primary),
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_BUTTON_DATA, &[(0, 1)]), buttons_secondary),
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_SETTINGS, &[(0, 0)]), settings),
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_SETTINGS, &[(0, 2)]), settings_xy),
            /* SEPARATE_LEDS: one query per LED, reply always in slot 0. */
            MockExchange::expect_reply(
                req_bytes(ASUS_CMD_GET_LED_DATA, &[(0, 0)]),
                ok_resp(&[(2, 2), (3, 200)]), /* cycle, raw brightness 200 */
            ),
            MockExchange::expect_reply(
                req_bytes(ASUS_CMD_GET_LED_DATA, &[(0, 1)]),
                ok_resp(&[(2, 0), (3, 100)]),
            ),
            MockExchange::expect_reply(
                req_bytes(ASUS_CMD_GET_LED_DATA, &[(0, 2)]),
                ok_resp(&[(2, 1), (3, 50)]),
            ),
        ]);

        let mut drv = AsusDriver::new();
        drv.load_profiles(&mut io, &mut info).await.unwrap();
        assert!(handle.script_exhausted());

        let p = &info.profiles[0];

        /* SEPARATE_XY_DPI + DOUBLE_DPI applied per axis. */
        assert!(matches!(p.resolutions[0].dpi, Dpi::Separate { x: 10000, y: 20000 }));
        assert!(matches!(p.resolutions[1].dpi, Dpi::Separate { x: 4500, y: 5500 }));
        assert_eq!(p.report_rate, 1000);

        /* RAW_BRIGHTNESS: hardware byte passed through unscaled. */
        assert_eq!(p.leds[0].mode, LedMode::Cycle);
        assert_eq!(p.leds[0].brightness, 200);
        assert_eq!(p.leds[1].brightness, 100);
        assert_eq!(p.leds[2].brightness, 50);

        /* Mapping order: [f0,f1,f2,e8,e9,e6,ea,eb,ec,ed,d0,d1,d2,d3].
         * Button 6 is side button A (secondary group), button 10 joystick up. */
        assert_eq!(p.buttons[6].action_type, At::Key);
        assert_eq!(p.buttons[6].mapping_value, KEY_SPACE);
        assert_eq!(p.buttons[10].action_type, At::Special);
        assert_eq!(p.buttons[10].mapping_value, 0); /* joystick placeholder */
    }

    /* Commit must write the dirty profile while it is selected, persist it,
     * and leave the hardware on the profile marked active in DeviceInfo. */
    #[tokio::test]
    async fn commit_writes_dirty_profile_and_switches_to_active() {
        let mut info = make_info(3, 2, 1, 2, &[], &[], &[]);
        info.profiles[1].is_dirty = true;
        info.profiles[1].buttons[0].action_type = At::None;
        info.profiles[1].buttons[1].action_type = At::Key;
        info.profiles[1].buttons[1].mapping_value = KEY_A;
        /* User switched the active profile to 2 via DBus SetActive. */
        info.profiles[0].is_active = false;
        info.profiles[2].is_active = true;

        let (mut io, handle) = DeviceIo::with_mock(vec![
            /* Hardware is currently on profile 0. */
            MockExchange::expect_reply(req_bytes(ASUS_CMD_GET_PROFILE_DATA, &[]), ok_resp(&[(8, 0)])),
            MockExchange::expect_reply(req_bytes(ASUS_CMD_SET_PROFILE, &[(0, 1)]), ok_resp(&[])),
            /* Button 0 disabled, button 1 bound to key A. */
            MockExchange::expect_reply(
                req_bytes(ASUS_CMD_SET_BUTTON, &[
                    (2, 0xf0), (3, ASUS_ACTION_TYPE_BUTTON),
                    (4, ASUS_BUTTON_CODE_DISABLED), (5, ASUS_ACTION_TYPE_BUTTON),
                ]),
                ok_resp(&[]),
            ),
            MockExchange::expect_reply(
                req_bytes(ASUS_CMD_SET_BUTTON, &[
                    (2, 0xf1), (3, ASUS_ACTION_TYPE_BUTTON),
                    (4, 0x04), (5, ASUS_ACTION_TYPE_KEY),
                ]),
                ok_resp(&[]),
            ),
            /* Polling rate 1000 Hz: field selector = dpi_count(2) + RATE(0). */
            MockExchange::expect_reply(
                req_bytes(ASUS_CMD_SET_SETTING, &[(0, 2), (2, 3)]),
                ok_resp(&[]),
            ),
            /* Two DPI presets at the from_entry default of 800 -> stored 15. */
            MockExchange::expect_reply(
                req_bytes(ASUS_CMD_SET_SETTING, &[(0, 0), (2, 15)]),
                ok_resp(&[]),
            ),
            MockExchange::expect_reply(
                req_bytes(ASUS_CMD_SET_SETTING, &[(0, 1), (2, 15)]),
                ok_resp(&[]),
            ),
            /* LED 0: mode Off is not an ASUS mode -> falls back to 0 (solid);
             * default brightness 255 -> hardware 4; black. */
            MockExchange::expect_reply(
                req_bytes(ASUS_CMD_SET_LED, &[(0, 0), (2, 0), (3, 4), (4, 0), (5, 0), (6, 0)]),
                ok_resp(&[]),
            ),
            MockExchange::expect_reply(req_bytes(ASUS_CMD_SAVE, &[]), ok_resp(&[])),
            /* DeviceInfo says profile 2 is active -> leave hardware there. */
            MockExchange::expect_reply(req_bytes(ASUS_CMD_SET_PROFILE, &[(0, 2)]), ok_resp(&[])),
        ]);

        let mut drv = AsusDriver::new();
        drv.init_from_config(&info.driver_config);
        drv.is_ready = true;

        drv.commit(&mut io, &info).await.expect("commit should succeed");
        assert!(handle.script_exhausted());
    }

    #[tokio::test]
    async fn commit_aborts_when_device_stays_asleep() {
        let (mut io, _handle) =
            DeviceIo::with_mock(vec![MockExchange::reply(asleep_resp())]);
        let info = make_info(3, 2, 1, 2, &[], &[], &[]);

        let mut drv = AsusDriver::new();
        drv.init_from_config(&info.driver_config);
        drv.is_ready = false;

        let err = drv.commit(&mut io, &info).await.unwrap_err();
        assert!(err.to_string().contains("sleeping"), "unexpected error: {err:#}");
        assert!(!drv.is_ready);
    }

    #[tokio::test]
    async fn commit_marks_device_not_ready_when_it_sleeps_mid_save() {
        /* Single profile, no buttons/LEDs: the first wire operation of the
         * save is the polling-rate write, which the device rejects. */
        let mut info = make_info(1, 0, 0, 2, &[], &[], &[]);
        info.profiles[0].is_dirty = true;

        let (mut io, _handle) =
            DeviceIo::with_mock(vec![MockExchange::reply(asleep_resp())]);

        let mut drv = AsusDriver::new();
        drv.init_from_config(&info.driver_config);
        drv.is_ready = true;

        assert!(drv.commit(&mut io, &info).await.is_err());
        assert!(!drv.is_ready, "driver must remember the device fell asleep");
    }
}
