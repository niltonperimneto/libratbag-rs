/* Logitech G600 gaming mouse driver.
 *
 * Targets the Logitech G600 MMO Gaming Mouse, a 20-button device with
 * 3 profiles, 4 DPI levels, and one RGB LED zone.
 *
 * Reference implementation: `src/driver-logitech-g600.c`. */

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::engine::device::{
    ActionType, Color, DeviceInfo, Dpi, LedMode, ProfileInfo, RgbColor,
    special_action,
};
use crate::hal::{DeviceDriver, DeviceIo};

/* ------------------------------------------------------------------ */
/* Protocol constants                                                   */
/* ------------------------------------------------------------------ */

const NUM_PROFILES: usize = 3;
const NUM_BUTTONS: usize = 41; /* 20 standard + 20 G-Shift + 1 color buffer */
const NUM_DPI: usize = 4;
const NUM_LED: usize = 1;

const DPI_MIN: u32 = 200;
const DPI_MAX: u32 = 8200;

/* HID report IDs */
const REPORT_ID_GET_ACTIVE: u8 = 0xF0;
const REPORT_ID_SET_ACTIVE: u8 = 0xF0;
const REPORT_ID_PROFILE_0: u8 = 0xF3;
const REPORT_ID_PROFILE_1: u8 = 0xF4;
const REPORT_ID_PROFILE_2: u8 = 0xF5;

/* Size of a full profile report (bytes). */
const REPORT_SIZE_PROFILE: usize = 154;

/* LED effect values */
const LED_SOLID: u8 = 0x00;
const LED_BREATHE: u8 = 0x01;
const LED_CYCLE: u8 = 0x02;

/* Supported report rates exposed to DBus/Piper (Hz). */
const REPORT_RATES: &[u32] = &[125, 142, 166, 200, 250, 333, 500, 1000];

/* ------------------------------------------------------------------ */
/* Button action mapping table                                          */
/* ------------------------------------------------------------------ */

/* Maps raw G600 button codes to (ActionType, mapping_value).
 * Matches C `logitech_g600_button_mapping[]` at driver-logitech-g600.c:105. */
const BUTTON_MAP: &[(u8, ActionType, u32)] = &[
    (0x01, ActionType::Button, 1),
    (0x02, ActionType::Button, 2),
    (0x03, ActionType::Button, 3),
    (0x04, ActionType::Button, 4),
    (0x05, ActionType::Button, 5),
    (0x11, ActionType::Special, special_action::RESOLUTION_UP),
    (0x12, ActionType::Special, special_action::RESOLUTION_DOWN),
    (0x13, ActionType::Special, special_action::RESOLUTION_CYCLE_UP),
    (0x14, ActionType::Special, special_action::PROFILE_CYCLE_UP),
    (0x15, ActionType::Special, special_action::RESOLUTION_ALTERNATE),
    (0x17, ActionType::Special, special_action::SECOND_MODE),
];

/* Look up a raw G600 button code in the mapping table. */
fn raw_to_button_action(code: u8) -> Option<(ActionType, u32)> {
    BUTTON_MAP.iter()
        .find(|(raw, _, _)| *raw == code)
        .map(|&(_, action, value)| (action, value))
}

/* Reverse-look up: find the raw code for a given action type and value.
 * Returns 0x00 if no match (unassigned). */
fn button_action_to_raw(action: ActionType, value: u32) -> u8 {
    BUTTON_MAP.iter()
        .find(|&&(_, a, v)| a == action && v == value)
        .map(|&(raw, _, _)| raw)
        .unwrap_or(0x00)
}

/* ------------------------------------------------------------------ */
/* Report data layouts                                                  */
/* ------------------------------------------------------------------ */

/* A single button entry in the profile report (3 bytes, packed). */
#[derive(Debug, Default, Clone, Copy)]
pub struct ButtonEntry {
    /* Action code — 0x01-0x05 mouse, 0x11-0x17 special, 0x00 key/disabled. */
    pub code: u8,
    /* Modifier byte (HID modifier bitmask for key actions). */
    pub modifier: u8,
    /* Keycode or button index. */
    pub key: u8,
}

/* Full profile report as it appears in the HID feature report.
 * Size must equal `REPORT_SIZE_PROFILE` (154 bytes). */
#[derive(Debug, Clone)]
pub struct ProfileReport {
    pub id: u8,
    pub led_red: u8,
    pub led_green: u8,
    pub led_blue: u8,
    pub led_effect: u8,
    /* LED animation duration (seconds, capped at 0x0f). */
    pub led_duration: u8,
    pub unknown1: [u8; 5],
    /* Polling frequency encoded as `frequency = 1000 / (value + 1)` Hz. */
    pub frequency: u8,
    /* DPI Shift mode resolution: `value * 50` from 200 to 8200; 0x00 = disabled. */
    pub dpi_shift: u8,
    /* Default DPI slot index (1-indexed, 1-4). */
    pub dpi_default: u8,
    /* DPI slot values: `value * 50` = actual DPI; 0x00 = disabled. */
    pub dpi: [u8; NUM_DPI],
    pub unknown2: [u8; 13],
    pub buttons: [ButtonEntry; 20],
    /* G-Shift mode color (R, G, B). */
    pub g_shift_color: [u8; 3],
    pub g_shift_buttons: [ButtonEntry; 20],
}

impl ProfileReport {
    fn new() -> Self {
        Self {
            id: 0,
            led_red: 0,
            led_green: 0,
            led_blue: 0,
            led_effect: 0,
            led_duration: 0,
            unknown1: [0; 5],
            frequency: 0,
            dpi_shift: 0,
            dpi_default: 1,
            dpi: [0; NUM_DPI],
            unknown2: [0; 13],
            buttons: [ButtonEntry::default(); 20],
            g_shift_color: [0; 3],
            g_shift_buttons: [ButtonEntry::default(); 20],
        }
    }

    fn from_bytes(b: &[u8; REPORT_SIZE_PROFILE]) -> Self {
        let mut s = Self::new();
        s.id = b[0];
        s.led_red = b[1];
        s.led_green = b[2];
        s.led_blue = b[3];
        s.led_effect = b[4];
        s.led_duration = b[5];
        s.unknown1.copy_from_slice(&b[6..11]);
        s.frequency = b[11];
        s.dpi_shift = b[12];
        s.dpi_default = b[13];
        s.dpi.copy_from_slice(&b[14..18]);
        s.unknown2.copy_from_slice(&b[18..31]);

        /* 20 normal buttons at offset 31, 3 bytes each */
        let mut off = 31;
        for btn in &mut s.buttons {
            btn.code = b[off];
            btn.modifier = b[off + 1];
            btn.key = b[off + 2];
            off += 3;
        }

        /* g_shift_color at offset 91 */
        s.g_shift_color.copy_from_slice(&b[91..94]);

        /* 20 g-shift buttons at offset 94, 3 bytes each */
        off = 94;
        for btn in &mut s.g_shift_buttons {
            btn.code = b[off];
            btn.modifier = b[off + 1];
            btn.key = b[off + 2];
            off += 3;
        }

        s
    }

    fn into_bytes(&self) -> [u8; REPORT_SIZE_PROFILE] {
        let mut b = [0u8; REPORT_SIZE_PROFILE];
        b[0] = self.id;
        b[1] = self.led_red;
        b[2] = self.led_green;
        b[3] = self.led_blue;
        b[4] = self.led_effect;
        b[5] = self.led_duration;
        b[6..11].copy_from_slice(&self.unknown1);
        b[11] = self.frequency;
        b[12] = self.dpi_shift;
        b[13] = self.dpi_default;
        b[14..18].copy_from_slice(&self.dpi);
        b[18..31].copy_from_slice(&self.unknown2);

        let mut off = 31;
        for btn in &self.buttons {
            b[off] = btn.code;
            b[off + 1] = btn.modifier;
            b[off + 2] = btn.key;
            off += 3;
        }

        b[91..94].copy_from_slice(&self.g_shift_color);

        off = 94;
        for btn in &self.g_shift_buttons {
            b[off] = btn.code;
            b[off + 1] = btn.modifier;
            b[off + 2] = btn.key;
            off += 3;
        }

        b
    }
}

/* Polled active-profile + resolution report. */
#[derive(Debug, Default, Clone, Copy)]
pub struct ActiveProfileReport {
    pub id: u8,
    /* Packed: `unknown1[0:0] | resolution[1:2] | unknown2[3:3] | profile[4:7]`. */
    pub packed: u8,
    pub unknown3: u8,
    pub unknown4: u8,
}

impl ActiveProfileReport {
    /* Extract the active profile index (0-based). */
    pub fn profile(&self) -> u8 {
        (self.packed >> 4) & 0x0f
    }

    /* Extract the active resolution index (0-based). */
    pub fn resolution(&self) -> u8 {
        (self.packed >> 1) & 0x03
    }
}

/* ------------------------------------------------------------------ */
/* DPI / frequency helpers                                              */
/* ------------------------------------------------------------------ */

/* Convert a DPI value to the raw byte sent in the profile report.
 * Raw = `dpi / 50`.  Range: 200 (0x04) - 8200 (0xa4). */
pub fn dpi_to_raw(dpi: u32) -> Option<u8> {
    if dpi < DPI_MIN || dpi > DPI_MAX || dpi % 50 != 0 {
        return None;
    }
    u8::try_from(dpi / 50).ok()
}

/* Decode the raw DPI byte to an actual DPI value. */
pub fn raw_to_dpi(raw: u8) -> u32 {
    u32::from(raw) * 50
}

/* Decode the frequency byte to Hz. */
pub fn raw_to_hz(raw: u8) -> u32 {
    if raw == 0 { 1000 } else { 1000 / (u32::from(raw) + 1) }
}

/* Encode Hz to the frequency byte. C: `(1000 / hz) - 1`. */
fn hz_to_raw(hz: u32) -> u8 {
    if hz == 0 { return 0; }
    ((1000 / hz).saturating_sub(1)).min(255) as u8
}

/* Generate the DPI list from DPI_MIN to DPI_MAX in steps of 50. */
fn dpi_range_list() -> Vec<u32> {
    (DPI_MIN..=DPI_MAX).step_by(50).collect()
}

/* ------------------------------------------------------------------ */
/* Cached state                                                         */
/* ------------------------------------------------------------------ */

#[derive(Debug)]
struct G600Data {
    profile_reports: [Option<ProfileReport>; NUM_PROFILES],
    active: ActiveProfileReport,
}

/* ------------------------------------------------------------------ */
/* Driver                                                               */
/* ------------------------------------------------------------------ */

pub struct LG600Driver {
    data: Option<G600Data>,
}

impl LG600Driver {
    pub fn new() -> Self {
        Self { data: None }
    }


}

/* Report IDs for the three profiles, indexed by profile number. */
const PROFILE_REPORT_IDS: [u8; NUM_PROFILES] = [
    REPORT_ID_PROFILE_0,
    REPORT_ID_PROFILE_1,
    REPORT_ID_PROFILE_2,
];

/* Read a button entry and convert to (ActionType, mapping_value).
 * C: logitech_g600_read_button (line 282). */
fn decode_button(entry: &ButtonEntry) -> (ActionType, u32) {
    if let Some((action, value)) = raw_to_button_action(entry.code) {
        return (action, value);
    }

    /* Code 0x00 with non-zero modifier/key = keyboard key.
     * C: lines 305-321 (macro from keycode). */
    if entry.code == 0x00 && (entry.modifier > 0 || entry.key > 0) {
        return (ActionType::Key, u32::from(entry.key));
    }

    (ActionType::None, 0)
}

/* Encode a button action back into a ButtonEntry for writing.
 * C: logitech_g600_write_profile button encoding (lines 506-531). */
fn encode_button(action: ActionType, value: u32) -> ButtonEntry {
    match action {
        ActionType::Button | ActionType::Special => {
            let code = button_action_to_raw(action, value);
            ButtonEntry { code, modifier: 0, key: 0 }
        }
        ActionType::Key | ActionType::Macro => {
            ButtonEntry { code: 0x00, modifier: 0, key: value as u8 }
        }
        _ => ButtonEntry::default(),
    }
}

#[async_trait]
impl DeviceDriver for LG600Driver {
    fn name(&self) -> &str {
        "Logitech G600"
    }

    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()> {
        /* Read the active profile report to confirm the device responds.
         * C: logitech_g600_get_active_profile_and_resolution (line 195). */
        let mut active_buf = [0u8; 4];
        active_buf[0] = REPORT_ID_GET_ACTIVE;
        io.get_feature_report(&mut active_buf)
            .map_err(anyhow::Error::from)
            .context("G600: failed to read active profile report")?;

        let active = ActiveProfileReport {
            id: active_buf[0],
            packed: active_buf[1],
            unknown3: active_buf[2],
            unknown4: active_buf[3],
        };

        info!(
            "G600: active profile={} resolution={}",
            active.profile(),
            active.resolution()
        );

        /* Read all three profile reports (154 bytes each).
         * C: logitech_g600_read_profile (line 361). */
        let mut profile_reports: [Option<ProfileReport>; NUM_PROFILES] = Default::default();

        for i in 0..NUM_PROFILES {
            let mut buf = [0u8; REPORT_SIZE_PROFILE];
            buf[0] = PROFILE_REPORT_IDS[i];

            io.get_feature_report(&mut buf)
                .map_err(anyhow::Error::from)
                .with_context(|| format!("G600: failed to read profile {i} report"))?;

            profile_reports[i] = Some(ProfileReport::from_bytes(&buf));
            debug!("G600: read profile {i} (report ID 0x{:02X})", PROFILE_REPORT_IDS[i]);
        }

        self.data = Some(G600Data { profile_reports, active });
        Ok(())
    }

    async fn load_profiles(&mut self, _io: &mut DeviceIo, info: &mut DeviceInfo) -> Result<()> {
        let data = self.data.as_ref()
            .ok_or_else(|| anyhow::anyhow!("G600: probe() was not called before load_profiles"))?;

        let active_profile = data.active.profile() as usize;
        let active_resolution = data.active.resolution() as usize;
        let dpi_list = dpi_range_list();

        info.profiles.clear();

        for i in 0..NUM_PROFILES {
            let report = data.profile_reports[i].as_ref()
                .ok_or_else(|| anyhow::anyhow!("G600: profile {i} report not read during probe"))?;

            let is_active_profile = i == active_profile;

            /* --- Report rate --- */
            let report_rate = raw_to_hz(report.frequency);

            /* --- Resolutions (4 DPI slots) --- */
            let mut resolutions = Vec::with_capacity(NUM_DPI);
            for j in 0..NUM_DPI {
                let raw = report.dpi[j];
                let disabled = raw == 0;
                let dpi_val = if disabled { 0 } else { raw_to_dpi(raw) };
                let is_default = !disabled && j == (report.dpi_default as usize).wrapping_sub(1);
                let is_active_res = is_active_profile && j == active_resolution;

                resolutions.push(crate::engine::device::ResolutionInfo {
                    index: j as u32,
                    dpi: Dpi::Unified(dpi_val),
                    dpi_list: dpi_list.clone(),
                    capabilities: Vec::new(),
                    is_active: is_active_res,
                    is_default,
                    is_disabled: disabled,
                });
            }

            /* --- Buttons (41 total) --- */
            /* C lines 296-299: supported action types */
            let action_types = vec![
                ActionType::None as u32,
                ActionType::Button as u32,
                ActionType::Special as u32,
                ActionType::Macro as u32,
            ];

            let mut buttons = Vec::with_capacity(NUM_BUTTONS);
            for b in 0..NUM_BUTTONS {
                let (action_type, mapping_value) = if b < 20 {
                    decode_button(&report.buttons[b])
                } else if b == 20 {
                    /* G-shift color buffer slot — not a real button. */
                    (ActionType::None, 0)
                } else {
                    /* G-shift buttons: indices 21-40 → g_shift_buttons[0-19]. */
                    decode_button(&report.g_shift_buttons[b - 21])
                };

                buttons.push(crate::engine::device::ButtonInfo {
                    index: b as u32,
                    action_type,
                    action_types: action_types.clone(),
                    mapping_value,
                    macro_entries: Vec::new(),
                });
            }

            /* --- LED (1 zone) --- */
            let led_mode = match report.led_effect {
                LED_BREATHE => LedMode::Breathing,
                LED_CYCLE => LedMode::Cycle,
                _ => {
                    /* LED_SOLID (0x00): check if RGB is all zero → Off */
                    if report.led_red == 0 && report.led_green == 0 && report.led_blue == 0 {
                        LedMode::Off
                    } else {
                        LedMode::Solid
                    }
                }
            };

            let effect_duration = match report.led_effect {
                LED_BREATHE | LED_CYCLE => u32::from(report.led_duration) * 1000,
                _ => 0,
            };

            let led = crate::engine::device::LedInfo {
                index: 0,
                mode: led_mode,
                modes: vec![LedMode::Off, LedMode::Solid, LedMode::Breathing, LedMode::Cycle],
                color: Color::from_rgb(RgbColor {
                    r: report.led_red,
                    g: report.led_green,
                    b: report.led_blue,
                }),
                secondary_color: Color::default(),
                tertiary_color: Color::default(),
                color_depth: 0,
                effect_duration,
                brightness: 255,
            };

            let profile = ProfileInfo {
                index: i as u32,
                name: String::new(),
                is_active: is_active_profile,
                is_enabled: true,
                is_dirty: false,
                report_rate,
                report_rates: REPORT_RATES.to_vec(),
                angle_snapping: -1,
                debounce: -1,
                debounces: Vec::new(),
                capabilities: Vec::new(),
                resolutions,
                buttons,
                leds: vec![led],
            };

            info.profiles.push(profile);

            debug!(
                "G600: profile {i}: rate={}Hz dpi_default={} led={:?}",
                report_rate, report.dpi_default, led_mode
            );
        }

        info!("G600: loaded {} profiles ({} buttons, {} DPI slots, {} LED)",
              NUM_PROFILES, NUM_BUTTONS, NUM_DPI, NUM_LED);
        Ok(())
    }

    async fn commit(&mut self, io: &mut DeviceIo, info: &DeviceInfo) -> Result<()> {
        let data = self.data.as_mut()
            .ok_or_else(|| anyhow::anyhow!("G600: probe() was not called before commit"))?;

        for profile in &info.profiles {
            if !profile.is_dirty {
                continue;
            }

            let idx = profile.index as usize;
            if idx >= NUM_PROFILES {
                warn!("G600: profile index {} out of range, skipping", idx);
                continue;
            }

            /* Get or create the cached report for read-modify-write. */
            let report = data.profile_reports[idx]
                .get_or_insert_with(|| {
                    let mut r = ProfileReport::new();
                    r.id = PROFILE_REPORT_IDS[idx];
                    r
                });

            /* 1. Report rate: C line 494 */
            report.frequency = hz_to_raw(profile.report_rate);

            /* 2. DPI slots: C lines 496-504 */
            let mut active_res_index: u8 = 0;
            for res in &profile.resolutions {
                let r_idx = res.index as usize;
                if r_idx >= NUM_DPI {
                    continue;
                }

                let dpi_val = match res.dpi {
                    Dpi::Unified(v) => v,
                    Dpi::Separate { x, .. } => x,
                    Dpi::Unknown => 0,
                };

                report.dpi[r_idx] = if res.is_disabled || dpi_val == 0 {
                    0
                } else {
                    dpi_to_raw(dpi_val).unwrap_or_else(|| {
                        /* Clamp to nearest valid value. */
                        let clamped = dpi_val.clamp(DPI_MIN, DPI_MAX);
                        let rounded = (clamped / 50) * 50;
                        dpi_to_raw(rounded).unwrap_or(0)
                    })
                };

                if res.is_default {
                    report.dpi_default = (r_idx + 1) as u8; /* 1-indexed */
                }

                if profile.is_active && res.is_active {
                    active_res_index = r_idx as u8;
                }
            }

            /* 3. Buttons: C lines 506-531 */
            for btn in &profile.buttons {
                let b = btn.index as usize;

                if b < 20 {
                    report.buttons[b] = encode_button(btn.action_type, btn.mapping_value);
                } else if b == 20 {
                    /* G-shift color buffer — skip encoding. */
                } else if b <= 40 {
                    report.g_shift_buttons[b - 21] = encode_button(btn.action_type, btn.mapping_value);
                }
            }

            /* 4. LED: C lines 534-561 */
            if let Some(led) = profile.leds.first() {
                let c = led.color.to_rgb();
                match led.mode {
                    LedMode::Off => {
                        report.led_effect = LED_SOLID;
                        report.led_red = 0;
                        report.led_green = 0;
                        report.led_blue = 0;
                    }
                    LedMode::Solid => {
                        report.led_effect = LED_SOLID;
                        report.led_red = c.r;
                        report.led_green = c.g;
                        report.led_blue = c.b;
                    }
                    LedMode::Breathing => {
                        report.led_effect = LED_BREATHE;
                        report.led_red = c.r;
                        report.led_green = c.g;
                        report.led_blue = c.b;
                        report.led_duration = (led.effect_duration / 1000).min(0x0f) as u8;
                    }
                    LedMode::Cycle => {
                        report.led_effect = LED_CYCLE;
                        report.led_red = c.r;
                        report.led_green = c.g;
                        report.led_blue = c.b;
                        report.led_duration = (led.effect_duration / 1000).min(0x0f) as u8;
                    }
                    _ => {
                        /* Unsupported mode — fall back to solid. */
                        report.led_effect = LED_SOLID;
                        report.led_red = c.r;
                        report.led_green = c.g;
                        report.led_blue = c.b;
                    }
                }

                /* Copy main LED color to g-shift color: C lines 565-567. */
                report.g_shift_color = [report.led_red, report.led_green, report.led_blue];
            }

            /* 5. Serialize and send: C lines 569-575 */
            let bytes = report.into_bytes();
            io.set_feature_report(&bytes)
                .map_err(anyhow::Error::from)
                .with_context(|| format!("G600: failed to write profile {idx}"))?;

            debug!("G600: committed profile {idx}");

            /* 6. If this is the active profile, update hardware resolution.
             * C: lines 583-587. */
            if profile.is_active {
                let res_buf = [REPORT_ID_SET_ACTIVE, 0x40 | (active_res_index << 1), 0x00, 0x00];
                io.set_feature_report(&res_buf)
                    .map_err(anyhow::Error::from)
                    .context("G600: failed to set current resolution")?;
                debug!("G600: set active resolution to {active_res_index}");
            }
        }

        Ok(())
    }
}
