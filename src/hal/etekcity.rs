/// Etekcity/Redragon gaming mouse driver.
///
/// Targets mice using the Etekcity USB HID protocol: Redragon M709, Etekcity
/// Scroll 1, and similar devices.
///
/// # Status
/// **Stub** — protocol constants and data layout are complete, but
/// `probe`/`load_profiles`/`commit` are not yet implemented.
///
/// Reference implementation: `src/driver-etekcity.c`.
use anyhow::Result;
use async_trait::async_trait;

use crate::engine::device::DeviceInfo;
use crate::hal::{DeviceDriver, DeviceIo};

/* ------------------------------------------------------------------ */
/* Protocol constants                                                  */
/* ------------------------------------------------------------------ */

/// Maximum profile index (0-based).
const ETEKCITY_PROFILE_MAX: u8 = 4;
/// Number of programmable buttons.
const ETEKCITY_BUTTON_MAX: usize = 10;
/// Number of DPI slots per profile.
const ETEKCITY_NUM_DPI: usize = 6;

/* HID report IDs */
const ETEKCITY_REPORT_ID_CONFIGURE_PROFILE: u8 = 0x04;
const ETEKCITY_REPORT_ID_PROFILE: u8 = 0x05;
const ETEKCITY_REPORT_ID_SETTINGS: u8 = 0x06;
const ETEKCITY_REPORT_ID_KEY_MAPPING: u8 = 0x07;
const ETEKCITY_REPORT_ID_SPEED_SETTING: u8 = 0x08;
#[allow(dead_code)]
const ETEKCITY_REPORT_ID_MACRO: u8 = 0x09;

/* Report sizes in bytes */
const ETEKCITY_REPORT_SIZE_PROFILE: usize = 50;
const ETEKCITY_REPORT_SIZE_SETTINGS: usize = 40;
#[allow(dead_code)]
const ETEKCITY_REPORT_SIZE_SPEED_SETTING: usize = 6;
#[allow(dead_code)]
const ETEKCITY_REPORT_SIZE_MACRO: usize = 130;

/* Configuration subtypes for CONFIGURE_PROFILE */
const ETEKCITY_CONFIG_SETTINGS: u8 = 0x10;
const ETEKCITY_CONFIG_KEY_MAPPING: u8 = 0x20;

/// Maximum number of keycode events in a single macro.
const ETEKCITY_MAX_MACRO_LENGTH: usize = 50;

/// Button raw-to-action mapping table entry.
///
/// The raw byte read from the settings report maps to a logical action.
struct ButtonMapping {
    raw: u8,
    /// Logical action description (used for logging and capability reporting).
    description: &'static str,
}

/// Full raw→action map (mirrors `etekcity_button_mapping[]` in the C driver).
#[allow(dead_code)]
static BUTTON_MAP: &[ButtonMapping] = &[
    ButtonMapping { raw: 1,  description: "button(1)" },
    ButtonMapping { raw: 2,  description: "button(2)" },
    ButtonMapping { raw: 3,  description: "button(3)" },
    ButtonMapping { raw: 4,  description: "special(double-click)" },
    ButtonMapping { raw: 6,  description: "none" },
    ButtonMapping { raw: 7,  description: "button(4)" },
    ButtonMapping { raw: 8,  description: "button(5)" },
    ButtonMapping { raw: 9,  description: "special(wheel-up)" },
    ButtonMapping { raw: 10, description: "special(wheel-down)" },
    ButtonMapping { raw: 11, description: "special(wheel-left)" },
    ButtonMapping { raw: 12, description: "special(wheel-right)" },
    ButtonMapping { raw: 13, description: "special(dpi-cycle-up)" },
    ButtonMapping { raw: 14, description: "special(dpi-up)" },
    ButtonMapping { raw: 15, description: "special(dpi-down)" },
    ButtonMapping { raw: 16, description: "macro" },
    ButtonMapping { raw: 18, description: "special(profile-cycle-up)" },
    ButtonMapping { raw: 19, description: "special(profile-up)" },
    ButtonMapping { raw: 20, description: "special(profile-down)" },
    ButtonMapping { raw: 25, description: "key(KEY_CONFIG)" },
    ButtonMapping { raw: 26, description: "key(KEY_PREVIOUSSONG)" },
    ButtonMapping { raw: 27, description: "key(KEY_NEXTSONG)" },
    ButtonMapping { raw: 28, description: "key(KEY_PLAYPAUSE)" },
    ButtonMapping { raw: 29, description: "key(KEY_STOPCD)" },
    ButtonMapping { raw: 30, description: "key(KEY_MUTE)" },
    ButtonMapping { raw: 31, description: "key(KEY_VOLUMEUP)" },
    ButtonMapping { raw: 32, description: "key(KEY_VOLUMEDOWN)" },
    ButtonMapping { raw: 33, description: "key(KEY_CALC)" },
    ButtonMapping { raw: 34, description: "key(KEY_MAIL)" },
    ButtonMapping { raw: 35, description: "key(KEY_BOOKMARKS)" },
    ButtonMapping { raw: 36, description: "key(KEY_FORWARD)" },
    ButtonMapping { raw: 37, description: "key(KEY_BACK)" },
    ButtonMapping { raw: 38, description: "key(KEY_STOP)" },
    ButtonMapping { raw: 39, description: "key(KEY_FILE)" },
    ButtonMapping { raw: 40, description: "key(KEY_REFRESH)" },
    ButtonMapping { raw: 41, description: "key(KEY_HOMEPAGE)" },
    ButtonMapping { raw: 42, description: "key(KEY_SEARCH)" },
];

/* ------------------------------------------------------------------ */
/* In-memory device state (mirrors C `etekcity_data`)                  */
/* ------------------------------------------------------------------ */

/// Packed HID settings report (40 bytes) for a single profile.
#[derive(Debug, Default, Clone)]
pub struct SettingsReport {
    /// Report ID = `ETEKCITY_REPORT_ID_SETTINGS`.
    pub report_id: u8,
    pub twenty_eight: u8,
    pub profile_id: u8,
    pub x_sensitivity: u8, /* 0x0a = 0 */
    pub y_sensitivity: u8,
    pub dpi_mask: u8,
    pub xres: [u8; ETEKCITY_NUM_DPI],
    pub yres: [u8; ETEKCITY_NUM_DPI],
    pub current_dpi: u8,
    pub _padding1: [u8; 7],
    pub report_rate: u8,
    pub _padding2: [u8; 4],
    pub light: u8,
    pub light_heartbeat: u8,
    pub _padding3: [u8; 5],
}

/// Macro entry: one (keycode, flag) pair within a macro sequence.
#[derive(Debug, Default, Clone, Copy)]
pub struct MacroKey {
    pub keycode: u8,
    pub flag: u8,
}

/// Full device state cached after `probe()`.
#[derive(Debug)]
struct EtekcityData {
    /// Raw profile key-mapping reports. Index = profile number.
    profiles: Vec<[u8; ETEKCITY_REPORT_SIZE_PROFILE]>,
    /// Parsed settings for each profile.
    settings: Vec<SettingsReport>,
    /// Macro state: `[profile][button][key_index]`.
    macros: Vec<Vec<[MacroKey; ETEKCITY_MAX_MACRO_LENGTH]>>,
    /// Current speed-setting report.
    speed_setting: [u8; 6],
}

/* ------------------------------------------------------------------ */
/* Driver                                                               */
/* ------------------------------------------------------------------ */

pub struct EtekcityDriver {
    data: Option<EtekcityData>,
}

impl EtekcityDriver {
    pub fn new() -> Self {
        Self { data: None }
    }
}

#[async_trait]
impl DeviceDriver for EtekcityDriver {
    fn name(&self) -> &str {
        "Etekcity"
    }

    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()> {
        /* Query the current profile to confirm the device responds. */
        let mut buf = [0u8; 3];
        buf[0] = ETEKCITY_REPORT_ID_PROFILE;
        io.get_feature_report(&mut buf)
            .map_err(anyhow::Error::from)?;

        let num_profiles = (ETEKCITY_PROFILE_MAX + 1) as usize;
        self.data = Some(EtekcityData {
            profiles: vec![[0u8; ETEKCITY_REPORT_SIZE_PROFILE]; num_profiles],
            settings: vec![SettingsReport::default(); num_profiles],
            macros: vec![
                vec![[MacroKey::default(); ETEKCITY_MAX_MACRO_LENGTH]; ETEKCITY_BUTTON_MAX + 1];
                num_profiles
            ],
            speed_setting: [0u8; 6],
        });

        // TODO: read all profiles, settings and macros from hardware.
        anyhow::bail!("Etekcity driver: load_profiles not yet implemented in the Rust port");
    }

    async fn load_profiles(&mut self, _io: &mut DeviceIo, _info: &mut DeviceInfo) -> Result<()> {
        // TODO: parse `self.data` and fill `info.profiles`.
        anyhow::bail!("Etekcity driver: load_profiles not yet implemented in the Rust port");
    }

    async fn commit(&mut self, _io: &mut DeviceIo, _info: &DeviceInfo) -> Result<()> {
        // TODO: write dirty profiles back to hardware.
        anyhow::bail!("Etekcity driver: commit not yet implemented in the Rust port");
    }
}

/* ------------------------------------------------------------------ */
/* Helpers (wired into the full implementation)                         */
/* ------------------------------------------------------------------ */

/// Build the 3-byte "configure profile" feature report.
#[allow(dead_code)]
fn configure_profile_report(profile: u8, config_type: u8) -> [u8; 3] {
    [ETEKCITY_REPORT_ID_CONFIGURE_PROFILE, profile, config_type]
}

/// Build the 3-byte "set active profile" feature report.
#[allow(dead_code)]
fn set_active_profile_report(index: u8) -> [u8; 3] {
    [ETEKCITY_REPORT_ID_PROFILE, 0x03, index]
}

/// Convert raw button index to the storage offset in the profile report.
///
/// Buttons 0-7 map linearly; buttons 8-9 are offset by 5 (gap in protocol).
#[allow(dead_code)]
fn button_to_raw_index(button: usize) -> usize {
    if button < 8 { button } else { button + 5 }
}

/// Look up the description for a raw button value.
#[allow(dead_code)]
fn raw_to_description(raw: u8) -> Option<&'static str> {
    BUTTON_MAP.iter().find(|m| m.raw == raw).map(|m| m.description)
}
