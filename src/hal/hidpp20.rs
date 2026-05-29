/* Logitech HID++ 2.0 driver implementation. */
/*  */
/* HID++ 2.0 is the modern feature-based protocol used by most current */
/* Logitech gaming mice. Each capability is exposed as a numbered "feature" */
/* that must be discovered at probe time via the Root feature (0x0000). */

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::time::{Duration, sleep};
use tracing::{debug, info, trace, warn};

use crate::engine::device::{Color, DeviceInfo, Dpi, LedMode, ProfileInfo, RgbColor};
use crate::hal::DeviceIo;

use super::hidpp::{
    self, BUTTON_SUBTYPE_CONSUMER, BUTTON_SUBTYPE_KEYBOARD, BUTTON_SUBTYPE_MOUSE,
    BUTTON_TYPE_DISABLED, BUTTON_TYPE_HID, BUTTON_TYPE_MACRO, BUTTON_TYPE_SPECIAL,
    DEVICE_IDX_CORDED, DEVICE_IDX_RECEIVER, HidppReport, LED_HW_MODE_BREATHING,
    LED_HW_MODE_COLOR_WAVE, LED_HW_MODE_CYCLE, LED_HW_MODE_FIXED, LED_HW_MODE_OFF,
    LED_HW_MODE_STARLIGHT, PAGE_ADJUSTABLE_DPI, PAGE_ADJUSTABLE_REPORT_RATE,
    PAGE_COLOR_LED_EFFECTS, PAGE_ONBOARD_PROFILES, PAGE_RGB_EFFECTS, PAGE_SPECIAL_KEYS_BUTTONS,
    ROOT_FEATURE_INDEX, ROOT_FN_GET_FEATURE, ROOT_FN_GET_PROTOCOL_VERSION,
};

/* Software ID used in all our requests (arbitrary, identifies us) */
const SW_ID: u8 = 0x04;

/* Adjustable DPI (0x2201) function IDs */
const DPI_FN_GET_SENSOR_DPI_LIST: u8 = 0x01;
const DPI_FN_GET_SENSOR_DPI: u8 = 0x02;
const DPI_FN_SET_SENSOR_DPI: u8 = 0x03;

/* Adjustable Report Rate (0x8060) function IDs */
const RATE_FN_GET_REPORT_RATE_LIST: u8 = 0x00;
const RATE_FN_GET_REPORT_RATE: u8 = 0x01;

/* Color LED Effects (0x8070) function IDs.
 * C defines: GET_INFO=0x00, GET_ZONE_INFO=0x10, GET_ZONE_EFFECT_INFO=0x20,
 *            SET_ZONE_EFFECT=0x30, GET_ZONE_EFFECT=0xE0.
 * The address byte encodes (function << 4 | sw_id), so we store the function
 * number in the upper nibble position: 0x30 → fn 3, 0xE0 → fn 14. */
const LED_FN_GET_ZONE_EFFECT: u8 = 0x0E;
const LED_FN_SET_ZONE_EFFECT: u8 = 0x03;

/* Onboard Profiles (0x8100) function IDs.
 * C defines: GET_PROFILES_DESCR=0x00, SET_ONBOARD_MODE=0x10,
 * GET_ONBOARD_MODE=0x20, SET_CURRENT_PROFILE=0x30,
 * GET_CURRENT_PROFILE=0x40, MEMORY_READ=0x50,
 * MEMORY_ADDR_WRITE=0x60, MEMORY_WRITE=0x70,
 * MEMORY_WRITE_END=0x80. */
const PROFILES_FN_GET_PROFILES_DESCR: u8 = 0x00;
const PROFILES_FN_SET_MODE: u8 = 0x01;
const PROFILES_FN_GET_MODE: u8 = 0x02;
const PROFILES_FN_SET_CURRENT_PROFILE: u8 = 0x03;
const PROFILES_FN_GET_CURRENT_PROFILE: u8 = 0x04;
const PROFILES_FN_MEMORY_READ: u8 = 0x05;
const PROFILES_FN_MEMORY_ADDR_WRITE: u8 = 0x06;
const PROFILES_FN_MEMORY_WRITE: u8 = 0x07;
const PROFILES_FN_MEMORY_WRITE_END: u8 = 0x08;
const PROFILES_FN_GET_CURRENT_DPI_INDEX: u8 = 0x0B;
const PROFILES_FN_SET_CURRENT_DPI_INDEX: u8 = 0x0C;

/* Feature 0x1b04 (Special Keys / Reprogrammable Controls) function IDs. */
const SPECIAL_KEYS_FN_GET_COUNT: u8 = 0x00;

/* Action types a HID++ 2.0 button can be remapped to.  Mirrors the C driver:
 * a button binding may be a mouse button, keyboard key, special action, or
 * macro.  Exposed via the button's ActionTypes D-Bus property so clients
 * (Piper/Twister) know what mappings are offerable. */
const HIDPP20_BUTTON_ACTION_TYPES: &[u32] = &[
    crate::engine::device::ActionType::Button as u32,
    crate::engine::device::ActionType::Key as u32,
    crate::engine::device::ActionType::Special as u32,
    crate::engine::device::ActionType::Macro as u32,
];

/* Onboard profile sector addresses — must match the C constants
 * HIDPP20_USER_PROFILES_G402 and HIDPP20_ROM_PROFILES_G402. */
const USER_PROFILES_BASE: u16 = 0x0000;
const ROM_PROFILES_BASE: u16 = 0x0100;

/* Onboard profile mode values for PROFILES_FN_SET_MODE / GET_MODE.
 * Mode 1 = onboard (mouse runs stored profiles autonomously).
 * Mode 2 = host (software controls mouse via live feature requests).
 * C constant: HIDPP20_ONBOARD_MODE = 1. */
const ONBOARD_MODE_ONBOARD: u8 = 0x01;
const ONBOARD_MODE_HOST: u8 = 0x02;

/* EEPROM profile sector layout constants.
 * The C struct `hidpp20_profile` places leds[HIDPP20_LED_COUNT] at byte
 * offset 208 inside the 256-byte packed union.  Each LED occupies 11
 * bytes (`struct hidpp20_internal_led`). */
const EEPROM_LED_OFFSET: usize = 208;
const EEPROM_LED_SIZE: usize = 11;
const EEPROM_LED_COUNT: usize = 2;

/* A feature page → runtime index mapping for a known set of capabilities. */
#[derive(Debug, Default)]
struct FeatureMap {
    adjustable_dpi: Option<u8>,
    special_keys: Option<u8>,
    onboard_profiles: Option<u8>,
    color_led_effects: Option<u8>,
    rgb_effects: Option<u8>,
    report_rate: Option<u8>,
}

impl FeatureMap {
    /* Store a discovered feature index based on its page ID. */
    fn insert(&mut self, page: u16, index: u8) {
        match page {
            PAGE_ADJUSTABLE_DPI => self.adjustable_dpi = Some(index),
            PAGE_SPECIAL_KEYS_BUTTONS => self.special_keys = Some(index),
            PAGE_ONBOARD_PROFILES => self.onboard_profiles = Some(index),
            PAGE_COLOR_LED_EFFECTS => self.color_led_effects = Some(index),
            PAGE_RGB_EFFECTS => self.rgb_effects = Some(index),
            PAGE_ADJUSTABLE_REPORT_RATE => self.report_rate = Some(index),
            _ => {}
        }
    }
}

/* HID++ 2.0 Button Binding representation (4 bytes) */
#[derive(Debug, Clone, Copy, Default)]
pub struct Hidpp20ButtonBinding {
    pub button_type: u8,
    pub subtype: u8,
    pub control_id_or_macro_id: [u8; 2], // little endian
}

impl Hidpp20ButtonBinding {
    pub fn from_bytes(buf: &[u8; 4]) -> Self {
        let button_type = buf[0];
        let subtype = buf[1];
        let mut control_id_or_macro_id = [0u8; 2];
        control_id_or_macro_id.copy_from_slice(&buf[2..4]);
        Self {
            button_type,
            subtype,
            control_id_or_macro_id,
        }
    }

    pub fn into_bytes(self) -> [u8; 4] {
        let mut buf = [0u8; 4];
        buf[0] = self.button_type;
        buf[1] = self.subtype;
        buf[2..4].copy_from_slice(&self.control_id_or_macro_id);
        buf
    }

    pub fn to_action(self) -> crate::engine::device::ActionType {
        use crate::engine::device::ActionType;
        match self.button_type {
            BUTTON_TYPE_MACRO => ActionType::Macro,
            BUTTON_TYPE_HID => match self.subtype {
                BUTTON_SUBTYPE_MOUSE => ActionType::Button,
                BUTTON_SUBTYPE_KEYBOARD => ActionType::Key,
                BUTTON_SUBTYPE_CONSUMER => ActionType::Special,
                _ => ActionType::Unknown,
            },
            BUTTON_TYPE_SPECIAL => ActionType::Special,
            BUTTON_TYPE_DISABLED => ActionType::None,
            _ => ActionType::Unknown,
        }
    }

    pub fn from_action(action: crate::engine::device::ActionType, mapping_value: u32) -> Self {
        use crate::engine::device::ActionType;
        let mut button_type = BUTTON_TYPE_DISABLED;
        let mut subtype = 0;
        let mut control_id = 0u16;

        match action {
            ActionType::Macro => {
                button_type = BUTTON_TYPE_MACRO;
                control_id = mapping_value as u16;
            }
            ActionType::Button => {
                button_type = BUTTON_TYPE_HID;
                subtype = BUTTON_SUBTYPE_MOUSE;
                /* EEPROM stores a big-endian bit mask: bit (n-1) set = button n.
                 * This matches the C hidpp20_buttons_from_cpu encoding. */
                let mask: u16 = if mapping_value > 0 && mapping_value <= 16 {
                    1u16 << (mapping_value - 1)
                } else {
                    0
                };
                return Self {
                    button_type,
                    subtype,
                    control_id_or_macro_id: mask.to_be_bytes(),
                };
            }
            ActionType::Key => {
                button_type = BUTTON_TYPE_HID;
                subtype = BUTTON_SUBTYPE_KEYBOARD;
                control_id = mapping_value as u16;
            }
            ActionType::Special => {
                button_type = BUTTON_TYPE_SPECIAL;
                control_id = hidpp20_special_to_raw(mapping_value) as u16;
            }
            _ => {}
        }

        Self {
            button_type,
            subtype,
            control_id_or_macro_id: control_id.to_le_bytes(),
        }
    }
}

/* ---------------------------------------------------------------------- */
/* HID++ 2.0 special-action translation tables                            */
/*                                                                        */
/* The hardware stores small raw opcodes (0x01–0x0b) in the button        */
/* binding for BUTTON_TYPE_SPECIAL.  DBus clients (e.g. Piper) expect the */
/* canonical ratbag_button_action_special enum values (base = 1 << 30).   */
/* These two helpers mirror the C hidpp20_profiles_specials[] table.       */
/* ---------------------------------------------------------------------- */

/* Convert a raw HID++ 2.0 special opcode (0x00–0x0b) read from the
 * device into the canonical special_action constant for DBus exposure. */
fn hidpp20_raw_to_special(raw: u8) -> u32 {
    use crate::engine::device::special_action as sa;
    match raw {
        0x01 => sa::WHEEL_LEFT,
        0x02 => sa::WHEEL_RIGHT,
        0x03 => sa::RESOLUTION_UP,
        0x04 => sa::RESOLUTION_DOWN,
        0x05 => sa::RESOLUTION_CYCLE_UP,
        0x06 => sa::RESOLUTION_DEFAULT,
        0x07 => sa::RESOLUTION_ALTERNATE,
        0x08 => sa::PROFILE_UP,
        0x09 => sa::PROFILE_DOWN,
        0x0a => sa::PROFILE_CYCLE_UP,
        0x0b => sa::SECOND_MODE,
        _ => sa::UNKNOWN,
    }
}

/* Convert a canonical special_action constant back to the raw HID++ 2.0
 * opcode that the hardware expects when writing a button binding. */
fn hidpp20_special_to_raw(special: u32) -> u8 {
    use crate::engine::device::special_action as sa;
    match special {
        sa::WHEEL_LEFT => 0x01,
        sa::WHEEL_RIGHT => 0x02,
        sa::RESOLUTION_UP => 0x03,
        sa::RESOLUTION_DOWN => 0x04,
        sa::RESOLUTION_CYCLE_UP => 0x05,
        sa::RESOLUTION_DEFAULT => 0x06,
        sa::RESOLUTION_ALTERNATE => 0x07,
        sa::PROFILE_UP => 0x08,
        sa::PROFILE_DOWN => 0x09,
        sa::PROFILE_CYCLE_UP => 0x0a,
        sa::SECOND_MODE => 0x0b,
        _ => 0x00,
    }
}

/* Parse HID++ 2.0 DPI sensor list entries (big-endian u16 pairs).
 *
 * The `list_bytes` slice starts immediately after the sensorIndex byte
 * in the getSensorDPIList (fn=1) response.  Values are big-endian u16;
 * the list ends at the first 0x0000.
 *
 * A value >= 0xE000 is a range-step marker: step = value & 0x1FFF.
 * The preceding discrete entry is the range minimum and the following
 * entry is the range maximum.  Otherwise each entry is a discrete DPI
 * value.  This mirrors the C hidpp20_adjustable_dpi_get_sensors()
 * parsing logic. */
fn parse_dpi_list(list_bytes: &[u8]) -> Vec<u32> {
    let mut entries: Vec<u16> = Vec::new();
    for chunk in list_bytes.chunks_exact(2) {
        let val = u16::from_be_bytes([chunk[0], chunk[1]]);
        if val == 0 {
            break;
        }
        entries.push(val);
    }

    let mut dpi_list: Vec<u32> = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let val = entries[i];
        if val >= 0xE000 {
            let step = u32::from(val & 0x1FFF);
            let dpi_min = dpi_list.pop().unwrap_or(200);
            let dpi_max = if i + 1 < entries.len() {
                u32::from(entries[i + 1])
            } else {
                dpi_min
            };
            if step > 0 && dpi_max >= dpi_min {
                let mut v = dpi_min;
                while v <= dpi_max {
                    dpi_list.push(v);
                    v = v.saturating_add(step);
                }
            }
            i += 2;
        } else {
            dpi_list.push(u32::from(val));
            i += 1;
        }
    }

    dpi_list
}

/* Feature 0x8100: Onboard Profiles */
#[derive(Debug, Clone, Copy, Default)]
pub struct Hidpp20OnboardProfilesInfo {
    pub profile_count: u8,
    pub profile_count_oob: u8,
    pub button_count: u8,
    pub sector_size: [u8; 2], /* Big Endian u16 */
}

impl Hidpp20OnboardProfilesInfo {
    pub fn from_bytes(buf: &[u8; 16]) -> Self {
        /* Byte layout (see C struct hidpp20_onboard_profiles_desc):
         *   [0] memory_model      – unused
         *   [1] profile_format_id – unused
         *   [2] macro_format_id   – unused
         *   [3] profile_count
         *   [4] profile_count_oob
         *   [5] button_count
         *   [6] sector_count      – unused
         *   [7..9] sector_size    (BE u16)
         *   [9] mechanical_layout – unused
         *   [10..16] reserved     – unused
         */
        let profile_count = buf[3];
        let profile_count_oob = buf[4];
        let button_count = buf[5];
        let mut sector_size = [0u8; 2];
        sector_size.copy_from_slice(&buf[7..9]);
        Self {
            profile_count,
            profile_count_oob,
            button_count,
            sector_size,
        }
    }
    pub fn sector_size(&self) -> u16 {
        u16::from_be_bytes(self.sector_size)
    }
}

pub struct Hidpp20Driver {
    device_index: u8,
    features: FeatureMap,
    cached_onboard_info: Option<Hidpp20OnboardProfilesInfo>,
    /* Cached hardware report rate (in Hz) read at probe time, used to skip
     * redundant setReportRate calls that some firmware rejects. */
    cached_report_rate_hz: u32,
    /* Set when any onboard-profile sector CRC check fails; triggers a full
     * rewrite/rebuild attempt on the next commit. */
    needs_eeprom_repair: bool,
}

impl Hidpp20Driver {
    pub fn new() -> Self {
        Self {
            device_index: DEVICE_IDX_RECEIVER,
            features: FeatureMap::default(),
            cached_onboard_info: None,
            cached_report_rate_hz: 0,
            needs_eeprom_repair: false,
        }
    }

    /* Attempt a HID++ 2.0 protocol version probe at a specific device index. */
    /* Returns `Some((major, minor))` on success, `None` on timeout or error. */
    /*                                                                         */
    /* Uses a single attempt (max_attempts=1) because a responding device      */
    /* replies within milliseconds.  The 2-second read deadline is ample for   */
    /* even busy wireless links, while keeping the total probe phase short     */
    /* enough that the combined probe+load_profiles stays within the actor's   */
    /* timeout budget.                                                         */
    async fn try_probe_index(&self, io: &mut DeviceIo, idx: u8) -> Option<(u8, u8)> {
        let request = hidpp::build_hidpp20_request(
            idx,
            ROOT_FEATURE_INDEX,
            ROOT_FN_GET_PROTOCOL_VERSION,
            SW_ID,
            &[],
        );

        io.request(&request, 20, 1, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() {
                return None;
            }
            if !report.matches_hidpp20(idx, ROOT_FEATURE_INDEX) {
                return None;
            }
            if let HidppReport::Long { params, .. } = report {
                Some((params[0], params[1]))
            } else {
                None
            }
        })
        .await
        .ok()
    }

    /* Query the Root feature (0x0000, fn 0) to find the runtime index of */
    /* a given feature page. Returns `None` if the device does not support it. */
    async fn get_feature_index(&self, io: &mut DeviceIo, feature_page: u16) -> Result<Option<u8>> {
        let [hi, lo] = feature_page.to_be_bytes();

        let request = hidpp::build_hidpp20_request(
            self.device_index,
            ROOT_FEATURE_INDEX,
            ROOT_FN_GET_FEATURE,
            SW_ID,
            &[hi, lo],
        );

        let dev_idx = self.device_index;
        io.request(&request, 20, 3, move |buf| {
            let report = HidppReport::parse(buf)?;

            /* An error from the Root feature means the page is not supported. */
            if report
                .hidpp20_error_code(dev_idx, ROOT_FEATURE_INDEX)
                .is_some()
                || report.is_error()
            {
                return Some(None);
            }

            /* Accept both Long and Short responses for the Root feature. */
            if !report.matches_hidpp20(dev_idx, ROOT_FEATURE_INDEX) {
                return None;
            }
            let index = match &report {
                HidppReport::Long { params, .. } => params[0],
                HidppReport::Short { params, .. } => params[0],
            };
            Some(if index == 0 { None } else { Some(index) })
        })
        .await
        .with_context(|| format!("Feature lookup for 0x{feature_page:04X} failed"))
    }

    /* Send a HID++ 2.0 feature request and return the 16-byte response payload. */
    /*                                                                          */
    /* The matcher accepts:                                                     */
    /* - Long responses  → full 16-byte params returned directly.               */
    /* - Short responses → 3-byte params zero-padded to 16 bytes (some SET      */
    /*   commands on wireless devices acknowledge with short reports).           */
    /* - HID++ error responses (both Long 0xFF and Short 0x8F) → surfaced       */
    /*   immediately as `Err` with the decoded error name.                      */
    async fn feature_request(
        &self,
        io: &mut DeviceIo,
        feature_index: u8,
        function: u8,
        params: &[u8],
    ) -> Result<[u8; 16]> {
        let request =
            hidpp::build_hidpp20_request(self.device_index, feature_index, function, SW_ID, params);

        /* Response is either Ok(params) or Err(error_code). */
        enum Resp {
            Ok([u8; 16]),
            HidppErr(u8),
        }

        let dev_idx = self.device_index;
        let resp = io
            .request(&request, 20, 3, move |buf| {
                let report = HidppReport::parse(buf)?;

                /* 1. Check for HID++ error (Long 0xFF or Short 0x8F). */
                if let Some(code) = report.hidpp20_error_code(dev_idx, feature_index) {
                    return Some(Resp::HidppErr(code));
                }

                /* 2. Successful Long response. */
                if let HidppReport::Long {
                    device_index,
                    sub_id,
                    params,
                    ..
                } = &report
                {
                    if *device_index == dev_idx && *sub_id == feature_index {
                        return Some(Resp::Ok(*params));
                    }
                }

                /* 3. Successful Short response (SET acknowledgment). */
                if let HidppReport::Short {
                    device_index,
                    sub_id,
                    params,
                    ..
                } = &report
                {
                    if *device_index == dev_idx && *sub_id == feature_index {
                        let mut long_params = [0u8; 16];
                        long_params[..3].copy_from_slice(params);
                        return Some(Resp::Ok(long_params));
                    }
                }

                None
            })
            .await
            .with_context(|| {
                format!("Feature request (idx=0x{feature_index:02X}, fn={function}) failed")
            })?;

        match resp {
            Resp::Ok(p) => Ok(p),
            Resp::HidppErr(code) => {
                let name = hidpp::hidpp20_error_name(code);
                Err(anyhow::anyhow!(
                    "HID++ error {name} (0x{code:02X}) for feature 0x{feature_index:02X} fn={function}"
                ))
            }
        }
    }

    /* Query the number of reprogrammable controls exposed by the
     * Special Keys / Buttons feature (0x1b04, function getCount).
     *
     * This is the canonical button enumerator in the C driver
     * (hidpp20_1b04_get_controls).  We use it as the source of truth for how
     * many button objects to expose when the onboard-profiles descriptor does
     * not provide a usable button_count (e.g. the G305, whose EEPROM may be
     * uninitialised, or any host-managed device without 0x8100).
     *
     * Returns 0 when the feature is absent or the request fails. */
    async fn query_special_keys_count(&self, io: &mut DeviceIo) -> usize {
        let Some(idx) = self.features.special_keys else {
            return 0;
        };
        match self
            .feature_request(io, idx, SPECIAL_KEYS_FN_GET_COUNT, &[])
            .await
        {
            Ok(resp) => {
                let count = resp[0] as usize;
                info!("HID++ 2.0: special keys/buttons (0x1b04) reports {count} controls");
                count
            }
            Err(e) => {
                debug!("HID++ 2.0: failed to query special keys count: {e}");
                0
            }
        }
    }

    /* Send a HID++ 2.0 short (7-byte) feature request with parameters.
     *
     * The C driver sends SET_CURRENT_PROFILE, SET_CURRENT_DPI_INDEX, and
     * MEMORY_WRITE_END as short reports.  Some firmware silently drops long
     * reports for these commands, so matching the C behaviour is essential. */
    async fn short_feature_request_with_params(
        &self,
        io: &mut DeviceIo,
        feature_index: u8,
        function: u8,
        params: &[u8],
    ) -> Result<()> {
        let request = hidpp::build_hidpp20_short_request_with_params(
            self.device_index,
            feature_index,
            function,
            SW_ID,
            params,
        );

        enum Resp {
            Ok,
            HidppErr(u8),
        }

        let dev_idx = self.device_index;
        let resp = io
            .request(&request, 20, 3, move |buf| {
                let report = HidppReport::parse(buf)?;

                if let Some(code) =
                    report.hidpp20_error_code(dev_idx, feature_index)
                {
                    return Some(Resp::HidppErr(code));
                }

                match &report {
                    HidppReport::Long { device_index, sub_id, .. }
                    | HidppReport::Short { device_index, sub_id, .. }
                        if *device_index == dev_idx && *sub_id == feature_index =>
                    {
                        Some(Resp::Ok)
                    }
                    _ => None,
                }
            })
            .await
            .with_context(|| {
                format!(
                    "Short feature request with params (idx=0x{feature_index:02X}, fn={function}) failed"
                )
            })?;

        match resp {
            Resp::Ok => Ok(()),
            Resp::HidppErr(code) => {
                let name = hidpp::hidpp20_error_name(code);
                Err(anyhow::anyhow!(
                    "HID++ error {name} (0x{code:02X}) for feature 0x{feature_index:02X} fn={function}"
                ))
            }
        }
    }

    /* Discover all supported features and cache their runtime indices. */
    async fn discover_features(&mut self, io: &mut DeviceIo) -> Result<()> {
        const FEATURE_QUERIES: &[(u16, &str)] = &[
            (PAGE_ADJUSTABLE_DPI, "Adjustable DPI"),
            (PAGE_SPECIAL_KEYS_BUTTONS, "Special Keys/Buttons"),
            (PAGE_ONBOARD_PROFILES, "Onboard Profiles"),
            (PAGE_COLOR_LED_EFFECTS, "Color LED Effects"),
            (PAGE_RGB_EFFECTS, "RGB Effects"),
            (PAGE_ADJUSTABLE_REPORT_RATE, "Adjustable Report Rate"),
        ];

        let mut found_count: usize = 0;
        for &(page, name) in FEATURE_QUERIES {
            match self.get_feature_index(io, page).await {
                Ok(Some(idx)) => {
                    info!("  Feature {name} (0x{page:04X}) at index 0x{idx:02X}");
                    self.features.insert(page, idx);
                    found_count += 1;
                }
                Ok(None) => {
                    info!("  Feature {name} (0x{page:04X}) not supported");
                }
                Err(e) => {
                    warn!("  Feature {name} (0x{page:04X}) query failed: {e}");
                }
            }
        }

        info!("HID++ 2.0: discovered {found_count} features");

        Ok(())
    }

    /* ---------------------------------------------------------------------- */
    /* Sector Memory Operations (PAGE_ONBOARD_PROFILES 0x8100)                */
    /* ---------------------------------------------------------------------- */

    /* Verify the CRC-CCITT checksum stored in the last two bytes (big-endian)
     * of a sector buffer, matching the C hidpp20_onboard_profiles_is_sector_valid.
     * Returns true when the CRC matches; logs a warning when it does not.
     * A mismatch is non-fatal — callers log it and continue with the data,
     * the same behaviour the legacy C driver exhibited. */
    fn verify_sector_crc(sector: u16, data: &[u8]) -> bool {
        if data.len() < 2 {
            warn!(
                "HID++ 2.0: sector 0x{sector:04X}: too short to validate CRC ({} bytes)",
                data.len()
            );
            return false;
        }
        let crc_offset = data.len() - 2;
        let computed = hidpp::compute_ccitt_crc(&data[..crc_offset]);
        let stored = u16::from_be_bytes([data[crc_offset], data[crc_offset + 1]]);
        if computed != stored {
            warn!(
                "HID++ 2.0: sector 0x{sector:04X}: CRC mismatch \
                 (stored 0x{stored:04X}, computed 0x{computed:04X})"
            );
            false
        } else {
            debug!("HID++ 2.0: sector 0x{sector:04X}: CRC OK (0x{stored:04X})");
            true
        }
    }

    async fn read_sector(
        &self,
        io: &mut DeviceIo,
        idx: u8,
        sector_index: u16,
        read_offset: u16,
        size: u16,
    ) -> Result<Vec<u8>> {
        let mut result = Vec::with_capacity(size as usize);
        let mut current_offset = read_offset;
        let end_offset = read_offset + size;

        while current_offset < end_offset {
            /* Firmware returns ERR_INVALID_ARGUMENT when a read would start within
             * the last 16 bytes of the sector but extend beyond it.  Rewind to
             * sector_size - 16 for the final partial chunk (mirrors C behaviour). */
            let chunk_size = (end_offset - current_offset).min(16);
            let effective_offset = if chunk_size < 16 {
                end_offset.saturating_sub(16)
            } else {
                current_offset
            };

            trace!(
                "HID++ 2.0: read_sector 0x{sector_index:04X} \
                 offset=0x{effective_offset:04X} chunk={chunk_size}B"
            );

            let mut bytes = [0u8; 16];
            bytes[0..2].copy_from_slice(&sector_index.to_be_bytes());
            bytes[2..4].copy_from_slice(&effective_offset.to_be_bytes());

            let response = self
                .feature_request(io, idx, PROFILES_FN_MEMORY_READ, &bytes)
                .await
                .context("Failed to read sector chunk")?;

            if effective_offset == current_offset {
                result.extend_from_slice(&response[..chunk_size as usize]);
            } else {
                let start_idx = 16 - chunk_size as usize;
                result.extend_from_slice(&response[start_idx..]);
            }
            current_offset += chunk_size;
        }

        Ok(result)
    }

    async fn write_sector(
        &self,
        io: &mut DeviceIo,
        idx: u8,
        sector_index: u16,
        write_offset: u16,
        data: &[u8],
    ) -> Result<()> {
        const WRITE_RETRIES: usize = 3;

        for attempt in 0..WRITE_RETRIES {
            match self
                .write_sector_once(io, idx, sector_index, write_offset, data)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) if attempt + 1 < WRITE_RETRIES => {
                    warn!(
                        "HID++ 2.0: write_sector 0x{sector_index:04X} failed \
                         (attempt {} of {WRITE_RETRIES}): {e}",
                        attempt + 1,
                    );
                    /* Some receivers reject rapid successive memWrite bursts;
                     * brief backoff mirrors C driver's retry behaviour. */
                    sleep(Duration::from_millis(15 * (attempt as u64 + 1))).await;
                }
                Err(e) => return Err(e),
            }
        }

        /* All retries exhausted without error or success — should be unreachable
         * with WRITE_RETRIES >= 1, but satisfy the type checker. */
        Ok(())
    }

    async fn write_sector_once(
        &self,
        io: &mut DeviceIo,
        idx: u8,
        sector_index: u16,
        write_offset: u16,
        data: &[u8],
    ) -> Result<()> {
        let size = data.len() as u16;

        // Step 1: Write Start command
        let mut start_bytes = [0u8; 16];
        start_bytes[0..2].copy_from_slice(&sector_index.to_be_bytes());
        start_bytes[2..4].copy_from_slice(&write_offset.to_be_bytes()); // usually 0 for a full sector
        start_bytes[4..6].copy_from_slice(&size.to_be_bytes());

        // 1. Initiate Write Sequence
        self.feature_request(io, idx, PROFILES_FN_MEMORY_ADDR_WRITE, &start_bytes)
            .await
            .context("Failed to start sector write")?;

        // 2. Iterate and Write Data Chunks (16 bytes at a time)
        for chunk in data.chunks(16) {
            let mut payload = [0u8; 16];
            payload[..chunk.len()].copy_from_slice(chunk);
            self.feature_request(io, idx, PROFILES_FN_MEMORY_WRITE, &payload)
                .await
                .context("Failed to write sector chunk")?;
        }

        /* 3. Finalize Write — C sends a SHORT report with no parameters. */
        self.short_feature_request_with_params(io, idx, PROFILES_FN_MEMORY_WRITE_END, &[])
            .await
            .context("Failed to end sector write")?;

        Ok(())
    }

    /* Read DPI sensor information using feature 0x2201. */
    async fn read_dpi_info(&self, io: &mut DeviceIo, profile: &mut ProfileInfo) -> Result<()> {
        let Some(idx) = self.features.adjustable_dpi else {
            return Ok(());
        };

        let list_data = self
            .feature_request(io, idx, DPI_FN_GET_SENSOR_DPI_LIST, &[0])
            .await?;
        let dpi_list = parse_dpi_list(&list_data[1..]); /* skip sensor_index byte */

        debug!(
            "HID++ 2.0: sensor 0 DPI list ({} values): first={}, last={}",
            dpi_list.len(),
            dpi_list.first().unwrap_or(&0),
            dpi_list.last().unwrap_or(&0),
        );

        /* Read current DPI (fn=2, getSensorDPI). */
        let dpi_data = self
            .feature_request(io, idx, DPI_FN_GET_SENSOR_DPI, &[0])
            .await?;
        let current_dpi = u16::from_be_bytes([dpi_data[1], dpi_data[2]]);
        let default_dpi = u16::from_be_bytes([dpi_data[3], dpi_data[4]]);

        /* Apply the queried DPI list and current value to all resolutions. */
        for res in &mut profile.resolutions {
            if !dpi_list.is_empty() {
                res.dpi_list = dpi_list.clone();
            }
            if res.is_active {
                res.dpi = Dpi::Unified(u32::from(current_dpi));
            }
        }

        debug!("HID++ 2.0: sensor 0 current DPI = {current_dpi} (default = {default_dpi})");
        Ok(())
    }

    /* Read report rate using feature 0x8060. */
    async fn read_report_rate(
        &mut self,
        io: &mut DeviceIo,
        profile: &mut ProfileInfo,
    ) -> Result<()> {
        let Some(idx) = self.features.report_rate else {
            return Ok(());
        };

        let list_data = self
            .feature_request(io, idx, RATE_FN_GET_REPORT_RATE_LIST, &[])
            .await?;
        let rate_bitmap = list_data[0];

        profile.report_rates = (0..8u32)
            .filter(|bit| rate_bitmap & (1 << bit) != 0)
            .map(|bit| 1000 / (bit + 1))
            .collect();

        let rate_data = self
            .feature_request(io, idx, RATE_FN_GET_REPORT_RATE, &[])
            .await?;
        let current_rate_ms = u32::from(rate_data[0]);
        if current_rate_ms > 0 {
            profile.report_rate = 1000 / current_rate_ms;
            self.cached_report_rate_hz = profile.report_rate;
        }
        Ok(())
    }

    /* Read LED zone effect from the device using feature 0x8070. */
    async fn read_led_info(&self, io: &mut DeviceIo, profile: &mut ProfileInfo) -> Result<()> {
        let Some(idx) = self.features.color_led_effects else {
            return Ok(());
        };

        for led in &mut profile.leds {
            let zone_index = led.index as u8;
            let response = self
                .feature_request(io, idx, LED_FN_GET_ZONE_EFFECT, &[zone_index])
                .await?;

            if response[0] != zone_index {
                warn!(
                    "LED read: zone mismatch (expected {zone_index}, got {})",
                    response[0]
                );
                continue;
            }

            *led = Self::parse_eeprom_led(&response[1..12], led.index as usize);
        }

        Ok(())
    }

    /* Write LED zone effect to the device using feature 0x8070. */
    /* TriColor mode is routed through feature 0x8071 (RGB Effects) instead. */
    async fn write_led_info(&self, io: &mut DeviceIo, profile: &ProfileInfo) -> Result<()> {
        for led in &profile.leds {
            let zone_index = led.index as u8;

            if led.mode == LedMode::TriColor {
                /* TriColor uses 0x8071 RGB Effects with the multi-LED cluster pattern command. */
                let Some(idx) = self.features.rgb_effects else {
                    warn!("TriColor requested but device lacks RGB Effects (0x8071)");
                    continue;
                };
                let led_payload = hidpp::build_led_payload(led);
                let mut bytes = [0u8; 16];
                bytes[0] = zone_index;
                bytes[1..12].copy_from_slice(&led_payload);
                bytes[12] = 0x01; /* persist */
                /* Function 0x02 = setMultiLEDRGBClusterPattern on 0x8071. Note: C passes 13 bytes */
                self.feature_request(io, idx, 0x02, &bytes[0..13])
                    .await
                    .context("Failed to write TriColor multi-LED cluster pattern")?;
            } else {
                let Some(idx) = self.features.color_led_effects else {
                    warn!("Device lacks Color LED Effects (0x8070)");
                    continue;
                };
                let led_payload = hidpp::build_led_payload(led);
                let mut bytes = [0u8; 16];
                bytes[0] = zone_index;
                bytes[1..12].copy_from_slice(&led_payload);
                bytes[12] = 0x01; /* persist */
                self.feature_request(io, idx, LED_FN_SET_ZONE_EFFECT, &bytes[0..13])
                    .await
                    .context("Failed to write LED zone effect")?;
            }

            debug!(
                "HID++ 2.0: committed LED zone {zone_index} mode={:?}",
                led.mode
            );
        }

        Ok(())
    }

    /* Write DPI sensor information using feature 0x2201. */
    async fn write_dpi_info(&self, io: &mut DeviceIo, profile: &ProfileInfo) -> Result<()> {
        let Some(idx) = self.features.adjustable_dpi else {
            return Ok(());
        };

        if let Some(res) = profile.resolutions.iter().find(|r| r.is_active)
            && let Dpi::Unified(dpi_val) = res.dpi
        {
            let dpi_u16 = dpi_val.min(u32::from(u16::MAX)) as u16;
            let [hi, lo] = dpi_u16.to_be_bytes();
            /* setSensorDPI is fn=3; only sensor_index + dpi_hi + dpi_lo are needed */
            let response = self
                .feature_request(io, idx, DPI_FN_SET_SENSOR_DPI, &[0, hi, lo])
                .await
                .context("Failed to write DPI")?;
            let actual_dpi = u16::from_be_bytes([response[1], response[2]]);
            debug!(
                "HID++ 2.0: committed DPI = {} (device ack: {})",
                dpi_val, actual_dpi
            );
        }
        Ok(())
    }

    /* Write report rate using feature 0x8060. */
    async fn write_report_rate(&self, io: &mut DeviceIo, profile: &ProfileInfo) -> Result<()> {
        const RATE_FN_SET_REPORT_RATE: u8 = 0x02;

        let Some(idx) = self.features.report_rate else {
            return Ok(());
        };

        if profile.report_rate > 0 {
            /* Some firmware returns INVALID_ARGUMENT when asked to set the
             * rate that is already active. Skip the write when unchanged. */
            if profile.report_rate == self.cached_report_rate_hz {
                debug!(
                    "HID++ 2.0: report rate unchanged at {} Hz, skipping write",
                    profile.report_rate
                );
                return Ok(());
            }
            /* Clamp the ms-interval to u8 range; realistic rates (125–8000 Hz)
             * always produce values 1–8 so this is purely defensive. */
            let rate_ms = (1000 / profile.report_rate).min(u32::from(u8::MAX)) as u8;
            self.feature_request(io, idx, RATE_FN_SET_REPORT_RATE, &[rate_ms])
                .await
                .context("Failed to write report rate")?;
            debug!(
                "HID++ 2.0: committed report rate = {} Hz",
                profile.report_rate
            );
        }
        Ok(())
    }

    /* ---------------------------------------------------------------------- */
    /* Helpers: query device-wide capabilities for UI validation               */
    /* ---------------------------------------------------------------------- */

    /// Query the DPI sensor range/list via feature 0x2201 (Adjustable DPI).
    /// Returns the expanded list of supported DPI values, or `None` if the
    /// feature is absent.  This is device-wide information used for the UI
    /// (Piper) — it does NOT read the current DPI setting.
    async fn query_dpi_sensor_range(&self, io: &mut DeviceIo) -> Option<Vec<u32>> {
        let idx = self.features.adjustable_dpi?;

        let list_data = self
            .feature_request(io, idx, DPI_FN_GET_SENSOR_DPI_LIST, &[0])
            .await
            .ok()?;
        let dpi_list = parse_dpi_list(&list_data[1..]); /* skip sensor_index byte */

        debug!(
            "HID++ 2.0: sensor DPI range query -> {} values (min={}, max={})",
            dpi_list.len(),
            dpi_list.first().unwrap_or(&0),
            dpi_list.last().unwrap_or(&0),
        );

        if dpi_list.is_empty() {
            None
        } else {
            Some(dpi_list)
        }
    }

    /// Query the supported report rate list via feature 0x8060.
    /// Returns the list of supported rates in Hz, or `None` if absent.
    async fn query_report_rate_list(&self, io: &mut DeviceIo) -> Option<Vec<u32>> {
        let idx = self.features.report_rate?;

        let list_data = self
            .feature_request(io, idx, RATE_FN_GET_REPORT_RATE_LIST, &[])
            .await
            .ok()?;

        let rate_bitmap = list_data[0];

        let rates: Vec<u32> = (0..8u32)
            .filter(|bit| rate_bitmap & (1 << bit) != 0)
            .map(|bit| 1000 / (bit + 1))
            .collect();

        debug!("HID++ 2.0: report rate list query → {:?}", rates);

        if rates.is_empty() { None } else { Some(rates) }
    }

    /* ---------------------------------------------------------------------- */
    /* Helpers: parse / serialize EEPROM LED structs                           */
    /* ---------------------------------------------------------------------- */

    /// Parse a single 11-byte `hidpp20_internal_led` from the EEPROM sector
    /// into a `LedInfo`.  Layout (from hidpp20.h):
    ///   byte 0:    mode (LED_HW_MODE_*)
    ///   bytes 1-10: mode-specific effect union
    fn parse_eeprom_led(led_bytes: &[u8], led_index: usize) -> crate::engine::device::LedInfo {
        let mut led = crate::engine::device::LedInfo {
            index: led_index as u32,
            mode: LedMode::Off,
            modes: Vec::new(),
            color: Color::default(),
            secondary_color: Color::default(),
            tertiary_color: Color::default(),
            color_depth: 0,
            effect_duration: 0,
            brightness: 0,
        };

        if led_bytes.len() < 11 {
            return led;
        }

        let mode_byte = led_bytes[0];

        match mode_byte {
            LED_HW_MODE_OFF => {
                led.mode = LedMode::Off;
            }
            LED_HW_MODE_FIXED => {
                led.mode = LedMode::Solid;
                led.color = Color::from_rgb(RgbColor {
                    r: led_bytes[1],
                    g: led_bytes[2],
                    b: led_bytes[3],
                });
                /* led_bytes[4] = effect_id, usually 0 */
            }
            LED_HW_MODE_CYCLE => {
                led.mode = LedMode::Cycle;
                /* bytes 1-5 unused; period at bytes 6-7 (BE), intensity at byte 8 */
                led.effect_duration = u32::from(u16::from_be_bytes([led_bytes[6], led_bytes[7]]));
                led.brightness = u32::from(led_bytes[8]) * 255 / 100;
            }
            LED_HW_MODE_COLOR_WAVE => {
                led.mode = LedMode::ColorWave;
                led.effect_duration = u32::from(u16::from_be_bytes([led_bytes[6], led_bytes[7]]));
                led.brightness = u32::from(led_bytes[8]) * 255 / 100;
            }
            LED_HW_MODE_STARLIGHT => {
                led.mode = LedMode::Starlight;
                led.color = Color::from_rgb(RgbColor {
                    r: led_bytes[1],
                    g: led_bytes[2],
                    b: led_bytes[3],
                });
                led.secondary_color = Color::from_rgb(RgbColor {
                    r: led_bytes[4],
                    g: led_bytes[5],
                    b: led_bytes[6],
                });
            }
            LED_HW_MODE_BREATHING => {
                led.mode = LedMode::Breathing;
                led.color = Color::from_rgb(RgbColor {
                    r: led_bytes[1],
                    g: led_bytes[2],
                    b: led_bytes[3],
                });
                led.effect_duration = u32::from(u16::from_be_bytes([led_bytes[4], led_bytes[5]]));
                /* byte 6 = waveform */
                led.brightness = u32::from(led_bytes[7]) * 255 / 100;
            }
            _ => {
                debug!("EEPROM LED {led_index}: unknown mode 0x{mode_byte:02X}");
            }
        }

        debug!(
            "EEPROM LED {led_index}: mode={:?} color={:?}",
            led.mode, led.color
        );
        led
    }

    /// Serialize a `LedInfo` into an 11-byte EEPROM LED struct for writing
    /// back to the profile sector (offset 208).
    fn serialize_eeprom_led(led: &crate::engine::device::LedInfo) -> [u8; 11] {
        let mut buf = [0u8; 11];

        match led.mode {
            LedMode::Off => {
                buf[0] = LED_HW_MODE_OFF;
            }
            LedMode::Solid => {
                buf[0] = LED_HW_MODE_FIXED;
                let c = led.color.to_rgb();
                buf[1] = c.r;
                buf[2] = c.g;
                buf[3] = c.b;
            }
            LedMode::Cycle => {
                buf[0] = LED_HW_MODE_CYCLE;
                let period = led.effect_duration as u16;
                buf[6..8].copy_from_slice(&period.to_be_bytes());
                buf[8] = (led.brightness * 100 / 255) as u8;
            }
            LedMode::ColorWave => {
                buf[0] = LED_HW_MODE_COLOR_WAVE;
                let period = led.effect_duration as u16;
                buf[6..8].copy_from_slice(&period.to_be_bytes());
                buf[8] = (led.brightness * 100 / 255) as u8;
            }
            LedMode::Starlight => {
                buf[0] = LED_HW_MODE_STARLIGHT;
                let c = led.color.to_rgb();
                buf[1] = c.r;
                buf[2] = c.g;
                buf[3] = c.b;
                let sc = led.secondary_color.to_rgb();
                buf[4] = sc.r;
                buf[5] = sc.g;
                buf[6] = sc.b;
            }
            LedMode::Breathing => {
                buf[0] = LED_HW_MODE_BREATHING;
                let c = led.color.to_rgb();
                buf[1] = c.r;
                buf[2] = c.g;
                buf[3] = c.b;
                let period = led.effect_duration as u16;
                buf[4..6].copy_from_slice(&period.to_be_bytes());
                /* byte 6 = waveform, keep 0 */
                buf[7] = (led.brightness * 100 / 255) as u8;
            }
            _ => {
                /* TriColor or unknown — leave as OFF */
                buf[0] = LED_HW_MODE_OFF;
            }
        }

        buf
    }
}

#[async_trait]
impl super::DeviceDriver for Hidpp20Driver {
    fn name(&self) -> &str {
        "Logitech HID++ 2.0"
    }

    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()> {
        /* Try the corded device index first, then the wireless receiver index.
         *
         * Wired mice respond to 0xFF instantly; probing 0x01 first wastes up
         * to 2 seconds in a read timeout because no device is listening at
         * that index.  Wireless mice on a DJ-managed hidraw node get a fast
         * error or timeout at 0xFF (the response goes to the receiver's own
         * node, not the mouse's), then succeed at 0x01.  Either way the
         * worst-case penalty is one single-read timeout (~2 s) rather than
         * the previous four seconds. */
        const PROBE_INDICES: &[u8] = &[DEVICE_IDX_CORDED, DEVICE_IDX_RECEIVER];

        for &idx in PROBE_INDICES {
            if let Some((major, minor)) = self.try_probe_index(io, idx).await {
                self.device_index = idx;
                info!("HID++ 2.0 device detected at index 0x{idx:02X} (protocol {major}.{minor})");
                self.discover_features(io).await?;
                return Ok(());
            }
            debug!("HID++ 2.0 probe at index 0x{idx:02X}: no response");
        }

        anyhow::bail!(
            "HID++ 2.0 protocol version probe failed (tried indices: {:02X?})",
            PROBE_INDICES
        );
    }

    async fn load_profiles(&mut self, io: &mut DeviceIo, info: &mut DeviceInfo) -> Result<()> {
        let has_g305_quirk = info.driver_config.quirks.iter().any(|q| q == "G305");

        /* If the device has PAGE_ONBOARD_PROFILES (0x8100), we initialize based on hardware capacity */
        if let Some(idx) = self.features.onboard_profiles {
            info!("HID++ 2.0: onboard_profiles feature found at index 0x{idx:02X}");

            let desc_data = self
                .feature_request(io, idx, PROFILES_FN_GET_PROFILES_DESCR, &[])
                .await
                .context("Failed to get Onboard Profiles Description")?;

            info!("HID++ 2.0: raw descriptor bytes: {:02X?}", &desc_data[..16]);

            let desc = Hidpp20OnboardProfilesInfo::from_bytes(&desc_data);
            self.cached_onboard_info = Some(desc);

            /* Use profile_count directly from the descriptor, matching the
             * C driver at hidpp20.c:2289.  The profile_count_oob field is the
             * number of ROM profiles and must NOT be used as a fallback for
             * the total count — doing so caused the G305 to attempt reading
             * 100+ profiles when profile_count was 1 and profile_count_oob
             * held a large firmware value. */
            let mut profile_count = desc.profile_count as usize;
            if profile_count == 0 {
                profile_count = 1;
            }
            /* Sanity-cap: no Logitech device has more than 5 onboard profiles. */
            if profile_count > 5 {
                warn!(
                    "HID++ 2.0: descriptor reports implausible profile_count={}, capping to 5",
                    profile_count
                );
                profile_count = 5;
            }

            /* The onboard descriptor's button_count is the primary source, but
             * some firmware (notably the G305 with uninitialised EEPROM) reports
             * 0 here even though the device has remappable buttons.  Fall back to
             * the Special Keys/Buttons feature (0x1b04) count so the button
             * objects still get exposed on D-Bus. */
            let mut button_count = desc.button_count as usize;
            if button_count == 0 {
                let special_keys_count = self.query_special_keys_count(io).await;
                if special_keys_count > 0 {
                    info!(
                        "HID++ 2.0: onboard descriptor button_count=0; \
                         using 0x1b04 control count {special_keys_count} instead"
                    );
                    button_count = special_keys_count;
                }
            }

            info!(
                "HID++ 2.0: Hardware described profiles={} (oob={}) buttons={} sector_size={}",
                profile_count,
                desc.profile_count_oob,
                button_count,
                desc.sector_size()
            );

            /* ----------------------------------------------------------------
             * Ensure the device is in onboard mode before reading profiles.
             * The C driver calls hidpp20_onboard_profiles_get_onboard_mode()
             * and switches to HIDPP20_ONBOARD_MODE (1) if it is not already
             * there.  Without this step some firmware may return stale or
             * unexpected data from sector reads.
             * ---------------------------------------------------------------- */
            match self
                .feature_request(io, idx, PROFILES_FN_GET_MODE, &[])
                .await
            {
                Ok(mode_resp) => {
                    let current_mode = mode_resp[0];
                    info!("HID++ 2.0: current onboard mode = {current_mode}");
                    if current_mode != ONBOARD_MODE_ONBOARD {
                        info!("HID++ 2.0: switching to onboard mode (was {current_mode})");
                        if let Err(e) = self
                            .feature_request(io, idx, PROFILES_FN_SET_MODE, &[ONBOARD_MODE_ONBOARD])
                            .await
                        {
                            warn!("HID++ 2.0: failed to set onboard mode: {e}");
                        }
                    }
                }
                Err(e) => {
                    warn!("HID++ 2.0: failed to get onboard mode: {e} (continuing)");
                }
            }

            /* Resize the Ratbag device abstraction to exactly match the hardware capabilities */
            info.profiles
                .resize_with(profile_count, ProfileInfo::default);
            for (i, p) in info.profiles.iter_mut().enumerate() {
                p.index = i as u32;
                p.buttons
                    .resize_with(button_count, crate::engine::device::ButtonInfo::default);
                for (b_idx, b) in p.buttons.iter_mut().enumerate() {
                    b.index = b_idx as u32;
                    b.action_types = HIDPP20_BUTTON_ACTION_TYPES.to_vec();
                }
            }

            let sector_size = desc.sector_size();

            /* ----------------------------------------------------------------
             * Read the root profile directory sector (0x0000).
             *
             * The G305 has a firmware bug where it throws ERR_INVALID_ARGUMENT
             * when the user sector has never been written.  The C driver
             * handles this via HIDPP20_QUIRK_G305: on error, it sets
             * read_userdata = false and reads ROM profiles instead.  We
             * replicate this fallback here.
             * ---------------------------------------------------------------- */
            let (root_sector_data, read_userdata) = match self
                .read_sector(io, idx, USER_PROFILES_BASE, 0, sector_size)
                .await
            {
                Ok(data) => {
                    let crc_ok = Self::verify_sector_crc(USER_PROFILES_BASE, &data);
                    if !crc_ok {
                        self.needs_eeprom_repair = true;
                        warn!(
                            "HID++ 2.0: profile dictionary CRC invalid; \
                             will read ROM profiles instead of corrupted EEPROM"
                        );
                    }
                    (Some(data), crc_ok)
                }
                Err(e) => {
                    if has_g305_quirk {
                        info!(
                            "HID++ 2.0: G305 quirk — root sector read failed ({e}), \
                             falling back to ROM profiles"
                        );
                    } else {
                        warn!(
                            "HID++ 2.0: root sector read failed ({e}), \
                             falling back to ROM profiles"
                        );
                    }
                    (None, false)
                }
            };

            /* Build per-profile address/enabled metadata.
             * Initialize to 0/false, matching the C driver at hidpp20.c:2793-2796.
             * Pre-filling with USER_PROFILES_BASE | (i + 1) caused the G403 HERO
             * duplication bug: when the directory had fewer entries than
             * profile_count (breaking at 0xFFFF), the remaining pre-filled
             * addresses pointed to real EEPROM sectors that contained duplicate
             * data.  With 0-initialization, unset profiles are caught by the
             * existing `if addr == 0 { continue }` guard in the read loop below,
             * then fall through to ROM fallback in the else branch. */
            let mut profile_addrs: Vec<u16> = vec![0u16; profile_count];
            let mut profile_enabled: Vec<bool> = vec![false; profile_count];

            if read_userdata {
                if let Some(ref root_data) = root_sector_data {
                    for i in 0..profile_count {
                        let offset = i * 4;
                        if offset + 4 > root_data.len() {
                            break;
                        }

                        let addr = u16::from_be_bytes([root_data[offset], root_data[offset + 1]]);
                        if addr == 0xFFFF {
                            break;
                        }
                        if addr != 0 {
                            profile_addrs[i] = addr;
                        }
                        profile_enabled[i] = root_data[offset + 2] != 0;
                    }
                }
            } else {
                /* No valid user directory — use ROM profile addresses.
                 * The C driver uses HIDPP20_ROM_PROFILES_G402 + i + 1, and
                 * when i >= num_rom_profiles it reuses the first ROM profile. */
                let num_rom = desc.profile_count_oob as usize;
                for i in 0..profile_count {
                    let rom_idx = if num_rom > 0 && i < num_rom { i } else { 0 };
                    profile_addrs[i] = ROM_PROFILES_BASE | ((rom_idx as u16) + 1);
                    profile_enabled[i] = true;
                }
                info!(
                    "HID++ 2.0: using ROM profile addresses: {:04X?}",
                    profile_addrs
                );
            }

            let num_rom = desc.profile_count_oob as usize;

            for i in 0..profile_count {
                let addr = profile_addrs[i];
                let enabled = profile_enabled[i];

                if addr == 0xFFFF {
                    continue;
                }

                /* Try the user EEPROM address first.  If the address is 0
                 * (not in directory), or the read/CRC fails, fall through
                 * to ROM — matching the C driver at hidpp20.c:2847-2876. */
                let mut use_rom = addr == 0;
                let mut profile_data = Vec::new();

                if !use_rom {
                    match self.read_sector(io, idx, addr, 0, sector_size).await {
                        Ok(data) => {
                            if Self::verify_sector_crc(addr, &data) {
                                profile_data = data;
                            } else {
                                self.needs_eeprom_repair = true;
                                warn!(
                                    "HID++ 2.0: profile {i} sector 0x{addr:04X} has bad CRC; \
                                     falling back to ROM"
                                );
                                use_rom = true;
                            }
                        }
                        Err(e) => {
                            warn!(
                                "HID++ 2.0: failed to read profile sector 0x{addr:04X}: {e}; \
                                 falling back to ROM for profile {i}"
                            );
                            use_rom = true;
                        }
                    }
                }

                if use_rom {
                    /* The C driver reuses the first ROM profile when i >= num_rom_profiles. */
                    let rom_idx = if num_rom > 0 && i < num_rom { i } else { 0 };
                    let rom_addr = ROM_PROFILES_BASE | ((rom_idx as u16) + 1);
                    info!("HID++ 2.0: profile {i} using ROM address 0x{rom_addr:04X}");
                    match self.read_sector(io, idx, rom_addr, 0, sector_size).await {
                        Ok(data) => {
                            profile_data = data;
                        }
                        Err(e) => {
                            warn!(
                                "HID++ 2.0: failed to read ROM profile sector 0x{rom_addr:04X}: {e}; \
                                 skipping profile {i}"
                            );
                            continue;
                        }
                    }
                }

                let p = &mut info.profiles[i];
                p.is_enabled = enabled;

                /* --- Report rate (byte 0): stored as ms-interval, convert to Hz --- */
                if !profile_data.is_empty() && profile_data[0] > 0 {
                    p.report_rate = 1000 / (profile_data[0] as u32);
                    debug!(
                        "HID++ 2.0: profile {i} EEPROM report rate = {} Hz (interval {}ms)",
                        p.report_rate, profile_data[0]
                    );
                }

                /* --- DPI list (bytes 3-12): 5 entries × 2 bytes LE --- */
                /* A raw value of 0 means the resolution slot is disabled  */
                /* but the slot must still appear on DBus with             */
                /* IsDisabled = true, matching the C daemon's behaviour.   */
                /* Piper expects to see all slots so users can enable them. */
                let mut eeprom_dpis: Vec<(u32, bool)> = Vec::new();
                for d_idx in 0..5usize {
                    let d_off = 3 + d_idx * 2;
                    if d_off + 2 <= profile_data.len() {
                        let raw =
                            u16::from_le_bytes([profile_data[d_off], profile_data[d_off + 1]]);
                        if raw == 0 || raw == 0xFFFF {
                            eeprom_dpis.push((0, true));
                        } else {
                            eeprom_dpis.push((u32::from(raw), false));
                        }
                    }
                }

                /* Default-DPI index (byte 1) */
                let default_dpi_idx = if profile_data.len() > 1 {
                    profile_data[1] as usize
                } else {
                    0
                };

                if !eeprom_dpis.is_empty() {
                    debug!(
                        "HID++ 2.0: profile {i} EEPROM DPIs: {:?} (default idx {})",
                        eeprom_dpis, default_dpi_idx
                    );

                    /* Rebuild the resolutions list to match the EEPROM entries. */
                    p.resolutions.clear();
                    for (r_idx, &(dpi_val, disabled)) in eeprom_dpis.iter().enumerate() {
                        p.resolutions.push(crate::engine::device::ResolutionInfo {
                            index: r_idx as u32,
                            dpi: crate::engine::device::Dpi::Unified(dpi_val),
                            dpi_list: Vec::new(), /* filled later by read_dpi_info */
                            capabilities: Vec::new(),
                            is_active: !disabled && r_idx == default_dpi_idx,
                            is_default: !disabled && r_idx == default_dpi_idx,
                            is_disabled: disabled,
                        });
                    }
                }

                /* --- Buttons (offset 32, 4 bytes each) --- */
                let max_buttons = button_count.min(16);
                for b_idx in 0..max_buttons {
                    let btn_offset = 32 + (b_idx * 4);
                    if btn_offset + 4 <= profile_data.len() {
                        let mut binding_bytes = [0u8; 4];
                        binding_bytes.copy_from_slice(&profile_data[btn_offset..btn_offset + 4]);
                        let binding = Hidpp20ButtonBinding::from_bytes(&binding_bytes);

                        p.buttons[b_idx].action_type = binding.to_action();

                        /* EEPROM mouse buttons are stored as a big-endian bit mask
                         * (matching the C hidpp20_buttons_to_cpu / buttons_from_cpu).
                         * ffs(mask) gives the 1-based button ordinal. */
                        let raw_id = u16::from_be_bytes(binding.control_id_or_macro_id);
                        let mapping_value = match (binding.button_type, binding.subtype) {
                            /* EEPROM mouse buttons are stored as a big-endian bit mask.
                             * ffs(mask) gives the 1-based button ordinal. */
                            (BUTTON_TYPE_HID, BUTTON_SUBTYPE_MOUSE) => {
                                if raw_id > 0 {
                                    u32::from(raw_id.trailing_zeros()) + 1
                                } else {
                                    0
                                }
                            }
                            /* Translate the raw HID++ special opcode to the
                             * canonical special_action constant for DBus. */
                            (BUTTON_TYPE_SPECIAL, _) => hidpp20_raw_to_special(raw_id as u8),
                            _ => u32::from(raw_id),
                        };
                        p.buttons[b_idx].mapping_value = mapping_value;

                        debug!(
                            "HID++ 2.0: profile {i} button {b_idx}: \
                             type=0x{:02X} sub=0x{:02X} raw=[{:02X},{:02X}] \
                             → action={:?} mapping={mapping_value}",
                            binding.button_type,
                            binding.subtype,
                            binding.control_id_or_macro_id[0],
                            binding.control_id_or_macro_id[1],
                            p.buttons[b_idx].action_type
                        );
                    }
                }

                /* --- LEDs (offset 208, 2 × 11 bytes) --- *
                 * The C struct places leds[HIDPP20_LED_COUNT] at offset 208
                 * inside the 256-byte packed union.  Each LED is 11 bytes
                 * (hidpp20_internal_led).  Parse them into the profile. */
                p.leds.clear();
                for led_idx in 0..EEPROM_LED_COUNT {
                    let off = EEPROM_LED_OFFSET + led_idx * EEPROM_LED_SIZE;
                    if off + EEPROM_LED_SIZE <= profile_data.len() {
                        let led = Self::parse_eeprom_led(
                            &profile_data[off..off + EEPROM_LED_SIZE],
                            led_idx,
                        );
                        p.leds.push(led);
                    }
                }
            }
        } else {
            /* No onboard profiles feature — create a single host-managed profile. */
            info!("HID++ 2.0: no onboard profiles feature; using single host-managed profile");
            if info.profiles.is_empty() {
                info.profiles.push(ProfileInfo::default());
            }

            /* Without onboard profiles there is no descriptor button_count, so
             * enumerate buttons from the Special Keys/Buttons feature (0x1b04).
             * Size each profile's button list to match the hardware controls. */
            let button_count = self.query_special_keys_count(io).await;
            if button_count > 0 {
                for p in &mut info.profiles {
                    p.buttons
                        .resize_with(button_count, crate::engine::device::ButtonInfo::default);
                    for (b_idx, b) in p.buttons.iter_mut().enumerate() {
                        b.index = b_idx as u32;
                        b.action_types = HIDPP20_BUTTON_ACTION_TYPES.to_vec();
                    }
                }
            }
        }

        /* Query the hardware for which profile is currently active rather
         * than blindly assuming profile 0.  The C driver uses
         * hidpp20_onboard_profiles_get_current_profile() which returns a
         * 1-based sector index in parameters[1].  Fall back to profile 0
         * if the query fails (e.g. non-onboard-profiles device). */
        let active_profile_idx: u32 = if let Some(idx) = self.features.onboard_profiles {
            match self
                .feature_request(io, idx, PROFILES_FN_GET_CURRENT_PROFILE, &[])
                .await
            {
                Ok(resp) => {
                    /* resp[1] is the 1-based profile sector, convert to 0-based */
                    let sector = resp[1];
                    let zero_based = if sector > 0 { u32::from(sector) - 1 } else { 0 };
                    info!(
                        "HID++ 2.0: hardware reports active profile sector={sector}, index={zero_based}"
                    );
                    zero_based
                }
                Err(e) => {
                    warn!("HID++ 2.0: failed to get current profile: {e}, defaulting to 0");
                    0
                }
            }
        } else {
            0
        };

        for profile in &mut info.profiles {
            profile.is_active = profile.index == active_profile_idx;
        }
        if !info.profiles.iter().any(|p| p.is_active) {
            if let Some(first) = info.profiles.first_mut() {
                first.is_active = true;
            } else {
                warn!("HID++ 2.0: no profiles available after load");
            }
        }

        /* For the active profile, override the default_dpi_idx with the
         * hardware-reported current DPI index.  The EEPROM byte 1 is the
         * *default* index (the starting one after profile load), but the
         * user may have physically cycled DPIs via the mouse button.
         * C: hidpp20_onboard_profiles_get_current_dpi_index(). */
        if let Some(idx) = self.features.onboard_profiles {
            if let Some(active_p) = info.profiles.iter_mut().find(|p| p.is_active) {
                match self
                    .feature_request(io, idx, PROFILES_FN_GET_CURRENT_DPI_INDEX, &[])
                    .await
                {
                    Ok(resp) => {
                        let hw_dpi_idx = resp[0] as usize;
                        debug!(
                            "HID++ 2.0: hardware current DPI index = {} for active profile {}",
                            hw_dpi_idx, active_p.index
                        );
                        for res in &mut active_p.resolutions {
                            res.is_active = res.index as usize == hw_dpi_idx;
                        }
                    }
                    Err(e) => {
                        debug!("HID++ 2.0: failed to get current DPI index: {e}");
                    }
                }
            }
        }

        /* When onboard profiles are present, all per-profile values (DPI,
         * report rate, LEDs, buttons) were already read from the EEPROM
         * sectors above.  We only query the live features for:
         *   - DPI sensor list/range → used for UI validation in Piper
         *   - Report rate list → used for UI validation in Piper
         *
         * When onboard profiles are absent, we fall back to reading
         * everything from the live features instead. */
        if self.features.onboard_profiles.is_some() {
            /* Query sensor DPI list/range once and apply to all profiles
             * (the sensor capabilities are device-wide, not per-profile). */
            let dpi_range = self.query_dpi_sensor_range(io).await;
            let rate_list = self.query_report_rate_list(io).await;

            for profile in &mut info.profiles {
                if let Some(ref range) = dpi_range {
                    for res in &mut profile.resolutions {
                        res.dpi_list = range.clone();
                    }
                }
                if let Some(ref rates) = rate_list {
                    profile.report_rates = rates.clone();
                }
            }
        } else {
            /* Fallback: no onboard profiles — read everything from live
             * feature requests.  This only works for the single default
             * profile since live features reflect hardware state, not
             * stored profile state. */
            for profile in &mut info.profiles {
                if let Err(e) = self.read_dpi_info(io, profile).await {
                    warn!("Failed to read DPI for profile {}: {e}", profile.index);
                }
                if let Err(e) = self.read_report_rate(io, profile).await {
                    warn!(
                        "Failed to read report rate for profile {}: {e}",
                        profile.index
                    );
                }
                if let Err(e) = self.read_led_info(io, profile).await {
                    warn!("Failed to read LEDs for profile {}: {e}", profile.index);
                }
            }
        }

        info!("HID++ 2.0: loaded {} profiles", info.profiles.len());
        Ok(())
    }

    async fn commit(&mut self, io: &mut DeviceIo, info: &DeviceInfo) -> Result<()> {
        /* When onboard profiles (0x8100) are present the firmware reads all
         * per-profile settings (DPI, report rate, LEDs) from the EEPROM
         * sectors.  We must NOT call the live feature set commands
         * (setSensorDPI 0x2201, setReportRate 0x8060, setZoneEffect 0x8070)
         * because those immediately change hardware state — making it look
         * like a DPI switch instead of a profile switch.
         *
         * When onboard profiles are ABSENT we are in host-managed mode and
         * the live feature calls are the only way to change settings. */
        if self.features.onboard_profiles.is_none() {
            if let Some(profile) = info.profiles.iter().find(|p| p.is_active) {
                if let Err(e) = self.write_dpi_info(io, profile).await {
                    warn!("Failed to commit DPI for profile {}: {e:#}", profile.index);
                }
                if let Err(e) = self.write_report_rate(io, profile).await {
                    warn!(
                        "Failed to commit report rate for profile {}: {e:#}",
                        profile.index
                    );
                }
                if let Err(e) = self.write_led_info(io, profile).await {
                    warn!("Failed to commit LEDs for profile {}: {e:#}", profile.index);
                }
            }
        }

        // Onboard Profiles (0x8100) EEPROM commit logic
        if let Some(idx) = self.features.onboard_profiles {
            if let Some(desc) = self.cached_onboard_info {
                let sector_size = desc.sector_size();

                /* A sector_size of 0 means the device reported no writable EEPROM
                 * (seen on the G305 when flash has never been initialised).
                 * Attempting EEPROM writes with size=0 would panic on the first
                 * profile_data[0] access.  Instead, clear the spurious repair
                 * flag (there is nothing to repair) and fall back to the same
                 * live feature writes used by host-managed devices. */
                if sector_size == 0 {
                    warn!(
                        "HID++ 2.0: onboard profiles descriptor reports sector_size=0 \
                         (profiles={}, buttons={}) — EEPROM unavailable; \
                         falling back to live feature writes",
                        desc.profile_count, desc.button_count
                    );
                    self.needs_eeprom_repair = false;
                    if let Some(profile) = info.profiles.iter().find(|p| p.is_active) {
                        if let Err(e) = self.write_dpi_info(io, profile).await {
                            warn!("Failed to commit DPI via live write: {e:#}");
                        }
                        if let Err(e) = self.write_report_rate(io, profile).await {
                            warn!("Failed to commit report rate via live write: {e:#}");
                        }
                        if let Err(e) = self.write_led_info(io, profile).await {
                            warn!("Failed to commit LEDs via live write: {e:#}");
                        }
                    }
                    return Ok(());
                }

                let force_repair = self.needs_eeprom_repair;

                /* Switch to host mode before writing EEPROM. Firmware rejects
                 * memWrite calls while in onboard mode (INVALID_ARGUMENT). */
                if let Err(e) = self
                    .feature_request(io, idx, PROFILES_FN_SET_MODE, &[ONBOARD_MODE_HOST])
                    .await
                {
                    warn!("Failed to switch to host mode: {e:#}");
                }

                /* Write each dirty profile to its sector.  Like the legacy C
                 * driver (hidpp20_onboard_profiles_write_profile), the sector
                 * address is simply `profile_index + 1` (0-based index → sector
                 * 1, 2, 3 …).  We do NOT rely on the directory sector (0x0000)
                 * being valid before the first write — the G305 may have an
                 * uninitialised directory that throws ERR_INVALID_ARGUMENT. */
                let mut any_written = false;
                let mut last_err: Option<anyhow::Error> = None;
                for profile in &info.profiles {
                    if !profile.is_dirty && !force_repair {
                        continue;
                    }

                    /* C: sector = index + 1 */
                    let addr = (profile.index + 1) as u16;

                    /* Read existing sector to preserve unknown fields, then
                     * patch the fields ratbag manages.  If the read fails
                     * (e.g., uninitialised flash), start from an all-0xFF
                     * buffer matching C's memset approach.
                     *
                     * When force_repair is true the sector data is known-
                     * corrupted so there is nothing worth preserving — skip
                     * the read entirely and start from a clean 0xFF template.
                     * This saves sector_size/16 USB round-trips per profile. */
                    let mut profile_data = if force_repair {
                        vec![0xFFu8; sector_size as usize]
                    } else {
                        let mut data = self
                            .read_sector(io, idx, addr, 0, sector_size)
                            .await
                            .unwrap_or_else(|_| vec![0xFFu8; sector_size as usize]);
                        if data.len() < sector_size as usize {
                            data.resize(sector_size as usize, 0xFF);
                        }
                        data
                    };

                    /* 1. Report rate (byte 0): stored as ms-interval */
                    if profile.report_rate > 0 {
                        profile_data[0] =
                            (1000 / profile.report_rate).min(u32::from(u8::MAX)) as u8;
                    }

                    /* 2. Default-DPI index (byte 1) */
                    if let Some(def_idx) = profile.resolutions.iter().position(|r| r.is_default) {
                        profile_data[1] = def_idx as u8;
                    }

                    /* 3. DPI list (bytes 3-12, 5 × LE u16) */
                    for (i, res) in profile.resolutions.iter().enumerate().take(5) {
                        if let Dpi::Unified(val) = res.dpi {
                            let dpi_bytes = (val.min(u32::from(u16::MAX)) as u16).to_le_bytes();
                            profile_data[3 + i * 2] = dpi_bytes[0];
                            profile_data[3 + i * 2 + 1] = dpi_bytes[1];
                        }
                    }

                    /* 4. Buttons (offset 32, 4 bytes each) */
                    let max_buttons = desc.button_count.min(16) as usize;
                    for btn in &profile.buttons {
                        let b_idx = btn.index as usize;
                        if b_idx < max_buttons {
                            let btn_offset = 32 + b_idx * 4;
                            if btn_offset + 4 <= profile_data.len() {
                                let binding = Hidpp20ButtonBinding::from_action(
                                    btn.action_type,
                                    btn.mapping_value,
                                );
                                profile_data[btn_offset..btn_offset + 4]
                                    .copy_from_slice(&binding.into_bytes());
                            }
                        }
                    }

                    /* 5. LEDs (offset 208, 2 × 11 bytes) */
                    {
                        for led in &profile.leds {
                            let led_idx = led.index as usize;
                            if led_idx < EEPROM_LED_COUNT {
                                let off = EEPROM_LED_OFFSET + led_idx * EEPROM_LED_SIZE;
                                if off + EEPROM_LED_SIZE <= profile_data.len() {
                                    let led_data = Self::serialize_eeprom_led(led);
                                    profile_data[off..off + EEPROM_LED_SIZE]
                                        .copy_from_slice(&led_data);
                                }
                            }
                        }
                    }

                    /* 6. Recompute CRC (last 2 bytes, BE) */
                    let crc_offset = profile_data.len() - 2;
                    let crc = hidpp::compute_ccitt_crc(&profile_data[..crc_offset]);
                    let crc_bytes = crc.to_be_bytes();
                    profile_data[crc_offset] = crc_bytes[0];
                    profile_data[crc_offset + 1] = crc_bytes[1];

                    /* 7. Write sector */
                    match self.write_sector(io, idx, addr, 0, &profile_data).await {
                        Ok(()) => {
                            debug!(
                                "HID++ 2.0: committed profile {} → sector 0x{addr:04X}",
                                profile.index
                            );
                            any_written = true;
                        }
                        Err(e) => {
                            warn!(
                                "Failed to write EEPROM sector 0x{addr:04X} for profile {}: {e}",
                                profile.index
                            );
                            last_err = Some(e);
                        }
                    }
                }

                /* After writing profile sectors, rebuild the directory (sector
                 * 0x0000) — mirrors C's hidpp20_onboard_profiles_write_dict.
                 * Format: 4 bytes per profile [0x00, i+1, enabled, 0x00],
                 * followed by [0xFF, 0xFF, 0x00, 0x00], rest padded 0xFF,
                 * then CRC-CCITT in the last two bytes. */
                if any_written {
                    let mut dir = vec![0xFFu8; sector_size as usize];
                    let mut pos = 0usize;
                    for profile in &info.profiles {
                        if pos + 4 > dir.len().saturating_sub(2) {
                            break;
                        }
                        dir[pos] = 0x00;
                        dir[pos + 1] = (profile.index + 1) as u8;
                        dir[pos + 2] = u8::from(profile.is_enabled);
                        dir[pos + 3] = 0x00;
                        pos += 4;
                    }
                    /* End-of-directory marker */
                    if pos + 4 <= dir.len().saturating_sub(2) {
                        dir[pos] = 0xFF;
                        dir[pos + 1] = 0xFF;
                        dir[pos + 2] = 0x00;
                        dir[pos + 3] = 0x00;
                    }
                    /* CRC over the whole sector minus the last 2 bytes */
                    let dir_crc_off = dir.len() - 2;
                    let dir_crc = hidpp::compute_ccitt_crc(&dir[..dir_crc_off]);
                    let dir_crc_bytes = dir_crc.to_be_bytes();
                    dir[dir_crc_off] = dir_crc_bytes[0];
                    dir[dir_crc_off + 1] = dir_crc_bytes[1];

                    if let Err(e) = self.write_sector(io, idx, 0x0000, 0, &dir).await {
                        warn!("HID++ 2.0: failed to write profile directory: {e}");
                        last_err = Some(e);
                    } else {
                        debug!("HID++ 2.0: wrote profile directory (sector 0x0000)");
                    }
                }

                /* Switch back to onboard mode after EEPROM writes. */
                if let Err(e) = self
                    .feature_request(io, idx, PROFILES_FN_SET_MODE, &[ONBOARD_MODE_ONBOARD])
                    .await
                {
                    warn!("Failed to switch back to onboard mode: {e:#}");
                }

                if let Some(e) = last_err {
                    /* Keep the flag set so we retry on the next commit. */
                    self.needs_eeprom_repair = true;
                    return Err(e);
                }

                /* Successful rewrite clears the repair flag. */
                self.needs_eeprom_repair = false;

                /* Tell the hardware which profile is now active.  The C driver
                 * calls hidpp20_onboard_profiles_set_current_profile() which
                 * uses function 0x03 with parameters[1] = 1-based sector.
                 * Without this, the device stays on whichever profile the
                 * firmware last selected and Piper's profile switching has no
                 * effect on the actual hardware output. */
                if let Some(active) = info.profiles.iter().find(|p| p.is_active) {
                    let sector = (active.index + 1) as u8; /* 0-based → 1-based */
                    /* C driver uses REPORT_ID_SHORT for this command.
                     * Some firmware silently drops long reports here. */
                    if let Err(e) = self
                        .short_feature_request_with_params(
                            io,
                            idx,
                            PROFILES_FN_SET_CURRENT_PROFILE,
                            &[0x00, sector],
                        )
                        .await
                    {
                        warn!(
                            "HID++ 2.0: failed to set current profile to {} (sector {sector}): {e}",
                            active.index
                        );
                    } else {
                        debug!(
                            "HID++ 2.0: set current profile = {} (sector {sector})",
                            active.index
                        );
                    }

                    /* Also set the active DPI index within the profile.
                     * C: hidpp20_onboard_profiles_set_current_dpi_index()
                     * uses function 0x0C with parameters[0] = resolution index. */
                    if let Some(res) = active.resolutions.iter().find(|r| r.is_active) {
                        let dpi_idx = res.index as u8;
                        /* C driver uses REPORT_ID_SHORT for this command too. */
                        if let Err(e) = self
                            .short_feature_request_with_params(
                                io,
                                idx,
                                PROFILES_FN_SET_CURRENT_DPI_INDEX,
                                &[dpi_idx],
                            )
                            .await
                        {
                            warn!("HID++ 2.0: failed to set DPI index to {dpi_idx}: {e}");
                        } else {
                            debug!("HID++ 2.0: set current DPI index = {dpi_idx}");
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /* Handle unsolicited HID++ 2.0 hardware events.
     *
     * The most important event is a profile-switch notification from feature
     * 0x8100 (Onboard Profiles).  When the user presses a physical profile
     * button, the hardware sends an unsolicited report with the new active
     * profile sector.  We parse this and update `DeviceInfo` accordingly.
     *
     * Returns `true` if the event caused a state change that the actor
     * should propagate via DBus signals. */
    async fn handle_event(&mut self, report: &[u8], info: &mut DeviceInfo) -> Result<bool> {
        let Some(parsed) = HidppReport::parse(report) else {
            return Ok(false);
        };

        /* Extract common fields; we only care about reports addressed to our device. */
        let (dev_idx, sub_id, params) = match &parsed {
            HidppReport::Long {
                device_index,
                sub_id,
                params,
                ..
            } => (*device_index, *sub_id, &params[..]),
            HidppReport::Short {
                device_index,
                sub_id,
                params,
                ..
            } => (*device_index, *sub_id, &params[..]),
        };

        if dev_idx != self.device_index {
            return Ok(false);
        }

        /* Check if this is a notification from the Onboard Profiles feature. */
        if let Some(_onboard_idx) = self.features.onboard_profiles.filter(|&idx| sub_id == idx) {
            /* The function nibble is in the address byte (byte [3]).
             * For a profile-change notification, we expect the
             * GET_CURRENT_PROFILE function (0x04) as the response
             * function, with params[1] = 1-based sector index. */
            let function = (report[3] >> 4) & 0x0F;

            if function == PROFILES_FN_GET_CURRENT_PROFILE
                || function == PROFILES_FN_SET_CURRENT_PROFILE
            {
                let sector = if params.len() > 1 {
                    params[1]
                } else {
                    params[0]
                };
                if sector == 0 {
                    return Ok(false);
                }
                let new_profile_index = (sector - 1) as u32;

                let mut changed = false;
                for profile in &mut info.profiles {
                    let should_be_active = profile.index == new_profile_index;
                    if profile.is_active != should_be_active {
                        profile.is_active = should_be_active;
                        changed = true;
                    }
                }

                if changed {
                    debug!(
                        "HID++ 2.0: hardware profile switch detected -> profile {new_profile_index}"
                    );
                }

                return Ok(changed);
            }

            /* DPI index change notification. */
            if function == PROFILES_FN_GET_CURRENT_DPI_INDEX
                || function == PROFILES_FN_SET_CURRENT_DPI_INDEX
            {
                let dpi_idx = params[0] as u32;
                let mut changed = false;

                if let Some(active_profile) = info.profiles.iter_mut().find(|p| p.is_active) {
                    for res in &mut active_profile.resolutions {
                        let should_be_active = res.index == dpi_idx;
                        if res.is_active != should_be_active {
                            res.is_active = should_be_active;
                            changed = true;
                        }
                    }
                }

                if changed {
                    debug!("HID++ 2.0: hardware DPI index change detected -> index {dpi_idx}");
                }

                return Ok(changed);
            }
        }

        Ok(false)
    }
}
