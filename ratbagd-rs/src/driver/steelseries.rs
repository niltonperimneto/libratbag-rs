use anyhow::Result;
use async_trait::async_trait;
use tracing::{debug, warn};

use crate::device::DeviceInfo;
use crate::driver::{DeviceDriver, DeviceIo};

/* ---------------------------------------------------------------------- */
/* Constants                                                              */
/* ---------------------------------------------------------------------- */
const STEELSERIES_NUM_PROFILES: u8 = 1;
const STEELSERIES_NUM_DPI: u8 = 2;

const STEELSERIES_REPORT_SIZE_SHORT: usize = 32;
const STEELSERIES_REPORT_SIZE: usize = 64;
const STEELSERIES_REPORT_LONG_SIZE: usize = 262;

/* Opcodes - V1 Short */
const STEELSERIES_ID_DPI_SHORT: u8 = 0x03;
const STEELSERIES_ID_REPORT_RATE_SHORT: u8 = 0x04;
const STEELSERIES_ID_LED_EFFECT_SHORT: u8 = 0x07;
const STEELSERIES_ID_LED_COLOR_SHORT: u8 = 0x08;
const STEELSERIES_ID_SAVE_SHORT: u8 = 0x09;
const STEELSERIES_ID_FIRMWARE_PROTOCOL1: u8 = 0x10;

/* Opcodes - V2 */
const STEELSERIES_ID_BUTTONS: u8 = 0x31;
const STEELSERIES_ID_DPI: u8 = 0x53;
const STEELSERIES_ID_REPORT_RATE: u8 = 0x54;
const STEELSERIES_ID_LED: u8 = 0x5b;
const STEELSERIES_ID_SAVE: u8 = 0x59;
const STEELSERIES_ID_FIRMWARE_PROTOCOL2: u8 = 0x90;
const STEELSERIES_ID_SETTINGS: u8 = 0x92;

/* Opcodes - V3 */
const STEELSERIES_ID_DPI_PROTOCOL3: u8 = 0x03;
const STEELSERIES_ID_REPORT_RATE_PROTOCOL3: u8 = 0x04;
const STEELSERIES_ID_LED_PROTOCOL3: u8 = 0x05;
const STEELSERIES_ID_SAVE_PROTOCOL3: u8 = 0x09;
const STEELSERIES_ID_FIRMWARE_PROTOCOL3: u8 = 0x10;
const STEELSERIES_ID_SETTINGS_PROTOCOL3: u8 = 0x16;

/* Opcodes - V4 */
const STEELSERIES_ID_DPI_PROTOCOL4: u8 = 0x15;
const STEELSERIES_ID_REPORT_RATE_PROTOCOL4: u8 = 0x17;

/* Buttons */
const STEELSERIES_BUTTON_OFF: u8 = 0x00;
const STEELSERIES_BUTTON_RES_CYCLE: u8 = 0x30;
const STEELSERIES_BUTTON_WHEEL_UP: u8 = 0x31;
const STEELSERIES_BUTTON_WHEEL_DOWN: u8 = 0x32;
const STEELSERIES_BUTTON_KEY: u8 = 0x10;
const STEELSERIES_BUTTON_KBD: u8 = 0x51;

/* Button payload stride per button in the report (bytes) */
const STEELSERIES_BUTTON_SIZE_SENSEIRAW: usize = 3;
const STEELSERIES_BUTTON_SIZE_STANDARD: usize = 5;

/* DPI scaling: hardware stores (dpi / 100) - 1; marker byte used by V2/V3 */
const STEELSERIES_DPI_MAGIC_MARKER: u8 = 0x42;

/* ---------------------------------------------------------------------- */
/* Driver Instance                                                        */
/* ---------------------------------------------------------------------- */

pub struct SteelseriesDriver {
    version: u8,
}

impl SteelseriesDriver {
    pub fn new() -> Self {
        Self { version: 0 }
    }
}

#[async_trait]
impl DeviceDriver for SteelseriesDriver {
    fn name(&self) -> &str {
        "SteelSeries"
    }

    async fn probe(&mut self, _io: &mut DeviceIo) -> Result<()> {
        debug!("Probe called for SteelSeries dummy");
        /* We will extract version from DeviceInfo during load_profiles since probe doesn't give us */
        /* the DeviceDb mappings yet, or we'll assume it defaults to 1 until load_profiles provides it. */
        Ok(())
    }

    async fn load_profiles(&mut self, io: &mut DeviceIo, info: &mut DeviceInfo) -> Result<()> {
        if let Some(v) = info.driver_config.device_version {
            self.version = v as u8;
        } else {
            warn!("DeviceVersion not found in config, defaulting to 1");
            self.version = 1;
        }

        /* SteelSeries devices don't usually report their settings (they rely on software DBs). */
        /* Therefore `load_profiles` merely sets the basic skeleton structure natively. */
        let report_rates = vec![125, 250, 500, 1000];

        info.profiles.clear();
        for profile_id in 0..STEELSERIES_NUM_PROFILES {
            let mut profile = crate::device::ProfileInfo {
                index: profile_id as u32,
                name: format!("Profile {}", profile_id),
                is_active: true,
                is_enabled: true,
                is_dirty: false,
                report_rate: 1000,
                report_rates: report_rates.clone(),
                angle_snapping: 0,
                debounce: 0,
                debounces: vec![],
                resolutions: vec![],
                buttons: vec![],
                leds: vec![],
            };

            for res_id in 0..STEELSERIES_NUM_DPI {
                profile.resolutions.push(crate::device::ResolutionInfo {
                    index: res_id as u32,
                    is_active: res_id == 0,
                    is_default: res_id == 0,
                    dpi: crate::device::Dpi::Unified(800 * (res_id as u32 + 1)),
                    dpi_list: vec![],
                    capabilities: vec![],
                    is_disabled: false,
                });
            }

            for btn_id in 0..6 {
                profile.buttons.push(crate::device::ButtonInfo {
                    index: btn_id,
                    action_type: crate::device::ActionType::Button,
                    action_types: vec![],
                    mapping_value: btn_id as u32 + 1,
                    macro_entries: vec![],
                });
            }

            for led_id in 0..2 {
                profile.leds.push(crate::device::LedInfo {
                    index: led_id,
                    mode: crate::device::LedMode::Solid,
                    modes: vec![],
                    color: crate::device::Color {
                        red: 255,
                        green: 0,
                        blue: 0,
                    },
                    secondary_color: crate::device::Color {
                        red: 0,
                        green: 0,
                        blue: 0,
                    },
                    tertiary_color: crate::device::Color {
                        red: 0,
                        green: 0,
                        blue: 0,
                    },
                    color_depth: 3,
                    effect_duration: 1000,
                    brightness: 255,
                });
            }

            /* Attempt to override defaults by reading active hardware settings */
            if let Err(e) = self.read_settings(io, &mut profile).await {
                warn!("SteelSeries: failed to read hardware settings: {e}");
            }

            info.profiles.push(profile);
        }

        if let Ok(fw) = self.read_firmware_version(io).await {
            info.firmware_version = fw;
        }

        Ok(())
    }

    async fn commit(&mut self, io: &mut DeviceIo, info: &DeviceInfo) -> Result<()> {
        let profile = info
            .profiles
            .iter()
            .find(|p| p.is_active)
            .or_else(|| info.profiles.first())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No profiles found in DeviceInfo (SteelSeries hardware requires at least 1)"
                )
            })?;

        /* Write DPI */
        for res in &profile.resolutions {
            if res.is_active {
                self.write_dpi(io, res).await?;
                break;
            }
        }

        /* Write Buttons */
        self.write_buttons(io, profile, info).await?;

        /* Write LEDs */
        for led in &profile.leds {
            self.write_led(io, led).await?;
        }

        self.write_report_rate(io, profile.report_rate).await?;

        /* Write Save (EEPROM target) */
        self.write_save(io).await?;

        Ok(())
    }
}

/* ---------------------------------------------------------------------- */
/* Helper methods – all payloads built as explicit byte arrays            */
/* ---------------------------------------------------------------------- */

impl SteelseriesDriver {
    async fn write_dpi(
        &self,
        io: &mut DeviceIo,
        res: &crate::device::ResolutionInfo,
    ) -> Result<()> {
        let dpi_val = match res.dpi {
            crate::device::Dpi::Unified(d) => d,
            crate::device::Dpi::Separate { x, .. } => x,
            crate::device::Dpi::Unknown => 800,
        };
        let scaled = (dpi_val / 100).saturating_sub(1) as u8;
        let res_id = res.index as u8 + 1;

        match self.version {
            1 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE_SHORT];
                buf[0] = STEELSERIES_ID_DPI_SHORT;
                buf[1] = res_id;
                buf[2] = scaled;
                io.write_report(&buf).await
            }
            2 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
                buf[0] = STEELSERIES_ID_DPI;
                buf[2] = res_id;
                buf[3] = scaled;
                buf[6] = STEELSERIES_DPI_MAGIC_MARKER;
                io.write_report(&buf).await
            }
            3 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
                buf[0] = STEELSERIES_ID_DPI_PROTOCOL3;
                buf[2] = res_id;
                buf[3] = scaled;
                buf[5] = STEELSERIES_DPI_MAGIC_MARKER;
                io.write_report(&buf).await
            }
            4 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE_SHORT];
                buf[0] = STEELSERIES_ID_DPI_PROTOCOL4;
                buf[1] = res_id;
                buf[2] = scaled;
                io.write_report(&buf).await
            }
            _ => Ok(()),
        }
    }

    async fn write_buttons(
        &self,
        io: &mut DeviceIo,
        profile: &crate::device::ProfileInfo,
        info: &DeviceInfo,
    ) -> Result<()> {
        let mut buf = [0u8; STEELSERIES_REPORT_LONG_SIZE];
        buf[0] = STEELSERIES_ID_BUTTONS;

        let is_senseiraw = info
            .driver_config
            .quirks
            .iter()
            .any(|q| q == "STEELSERIES_QUIRK_SENSEIRAW");

        let button_size = if is_senseiraw { STEELSERIES_BUTTON_SIZE_SENSEIRAW } else { STEELSERIES_BUTTON_SIZE_STANDARD };
        let report_size = if is_senseiraw {
            STEELSERIES_REPORT_SIZE_SHORT
        } else {
            STEELSERIES_REPORT_LONG_SIZE
        };

        for button in &profile.buttons {
            let idx = 2 + (button.index as usize) * button_size;
            if idx >= report_size {
                continue;
            } /* Bounds guard */

            match button.action_type {
                crate::device::ActionType::Button => {
                    buf[idx] = button.mapping_value as u8;
                }
                crate::device::ActionType::Key => {
                    let hid_usage = (button.mapping_value % 256) as u8;

                    if is_senseiraw {
                        buf[idx] = STEELSERIES_BUTTON_KEY;
                        if idx + 1 < report_size {
                            buf[idx + 1] = hid_usage;
                        } else {
                            warn!("SteelSeries: button {} key data truncated (offset {} exceeds report size {})",
                                  button.index, idx + 1, report_size);
                        }
                    } else {
                        buf[idx] = STEELSERIES_BUTTON_KBD;
                        if idx + 1 < report_size {
                            buf[idx + 1] = hid_usage;
                        } else {
                            warn!("SteelSeries: button {} key data truncated (offset {} exceeds report size {})",
                                  button.index, idx + 1, report_size);
                        }
                    }
                }
                crate::device::ActionType::Macro => {
                    /* Extract modifiers and the final keycode from macro entries if simulating a key sequence */
                    let mut modifiers = 0u8;
                    let mut final_key = 0u8;

                    for &(ev_type, k) in &button.macro_entries {
                        if ev_type == 0 {
                            /* Press */
                            match k {
                                224 => {
                                    modifiers |= 0x01;
                                } /* LCTRL */
                                225 => {
                                    modifiers |= 0x02;
                                } /* LSHIFT */
                                226 => {
                                    modifiers |= 0x04;
                                } /* LALT */
                                227 => {
                                    modifiers |= 0x08;
                                } /* LMETA */
                                228 => {
                                    modifiers |= 0x10;
                                } /* RCTRL */
                                229 => {
                                    modifiers |= 0x20;
                                } /* RSHIFT */
                                230 => {
                                    modifiers |= 0x40;
                                } /* RALT */
                                231 => {
                                    modifiers |= 0x80;
                                } /* RMETA */
                                _ => final_key = (k % 256) as u8,
                            }
                        }
                    }

                    if is_senseiraw {
                        buf[idx] = STEELSERIES_BUTTON_KEY;
                        if idx + 1 < report_size {
                            buf[idx + 1] = final_key;
                        } else {
                            warn!("SteelSeries: button {} macro key truncated (offset {} exceeds report size {})",
                                  button.index, idx + 1, report_size);
                        }
                    } else {
                        buf[idx] = STEELSERIES_BUTTON_KBD;
                        let mut cursor = idx;

                        /* Maximum of 3 modifiers allowed by SteelSeries protocol natively */
                        static MODIFIER_TABLE: [(u8, u8); 8] = [
                            (0x01, 0xE0),
                            (0x02, 0xE1),
                            (0x04, 0xE2),
                            (0x08, 0xE3),
                            (0x10, 0xE4),
                            (0x20, 0xE5),
                            (0x40, 0xE6),
                            (0x80, 0xE7),
                        ];
                        for &(mask, code) in &MODIFIER_TABLE {
                            if (modifiers & mask) != 0 && cursor - idx < 3 {
                                if cursor + 1 < report_size {
                                    buf[cursor + 1] = code;
                                } else {
                                    warn!("SteelSeries: button {} modifier truncated (offset {} exceeds report size {})",
                                          button.index, cursor + 1, report_size);
                                }
                                cursor += 1;
                            }
                        }

                        if cursor + 1 < report_size {
                            buf[cursor + 1] = final_key;
                        } else {
                            warn!("SteelSeries: button {} macro final key truncated (offset {} exceeds report size {})",
                                  button.index, cursor + 1, report_size);
                        }
                    }
                }
                crate::device::ActionType::Special => {
                    /* Simple map for mapping_value -> RES_CYCLE etc... */
                    match button.mapping_value {
                        1 => buf[idx] = STEELSERIES_BUTTON_RES_CYCLE,
                        2 => buf[idx] = STEELSERIES_BUTTON_WHEEL_UP,
                        3 => buf[idx] = STEELSERIES_BUTTON_WHEEL_DOWN,
                        _ => buf[idx] = STEELSERIES_BUTTON_OFF,
                    }
                }
                _ => buf[idx] = STEELSERIES_BUTTON_OFF,
            }
        }

        if self.version == 3 {
            io.set_feature_report(&buf[..report_size])?;
            Ok(())
        } else {
            io.write_report(&buf[..report_size]).await
        }
    }

    async fn write_report_rate(&self, io: &mut DeviceIo, hz: u32) -> Result<()> {
        let rate_val = (1000 / std::cmp::max(hz, 125)) as u8;

        match self.version {
            1 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE_SHORT];
                buf[0] = STEELSERIES_ID_REPORT_RATE_SHORT;
                buf[2] = rate_val;
                io.write_report(&buf).await
            }
            2 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
                buf[0] = STEELSERIES_ID_REPORT_RATE;
                buf[2] = rate_val;
                io.write_report(&buf).await
            }
            3 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
                buf[0] = STEELSERIES_ID_REPORT_RATE_PROTOCOL3;
                buf[2] = rate_val;
                io.write_report(&buf).await
            }
            4 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE_SHORT];
                buf[0] = STEELSERIES_ID_REPORT_RATE_PROTOCOL4;
                buf[2] = rate_val;
                io.write_report(&buf).await
            }
            _ => Ok(()),
        }
    }

    async fn write_led(&self, io: &mut DeviceIo, led: &crate::device::LedInfo) -> Result<()> {
        match self.version {
            1 => self.write_led_v1(io, led).await,
            2 => self.write_led_v2(io, led).await,
            3 => self.write_led_v3(io, led).await,
            _ => Ok(()), /* Protocol 4 etc untested for LED parity here */
        }
    }

    async fn write_led_v1(&self, io: &mut DeviceIo, led: &crate::device::LedInfo) -> Result<()> {
        let effect = match led.mode {
            crate::device::LedMode::Off | crate::device::LedMode::Solid => 0x01,
            crate::device::LedMode::Breathing => {
                let ms = led.effect_duration;
                if ms <= 3000 {
                    0x04
                } else if ms <= 5000 {
                    0x03
                } else {
                    0x02
                }
            }
            _ => return Ok(()),
        };

        /* Effect report: [report_id, led_id, effect, ...padding] */
        let mut effect_buf = [0u8; STEELSERIES_REPORT_SIZE_SHORT];
        effect_buf[0] = STEELSERIES_ID_LED_EFFECT_SHORT;
        effect_buf[1] = led.index as u8 + 1;
        effect_buf[2] = effect;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        io.write_report(&effect_buf).await?;

        /* Color report: [report_id, led_id, r, g, b, ...padding] */
        let mut color_buf = [0u8; STEELSERIES_REPORT_SIZE_SHORT];
        color_buf[0] = STEELSERIES_ID_LED_COLOR_SHORT;
        color_buf[1] = led.index as u8 + 1;
        color_buf[2] = led.color.red as u8;
        color_buf[3] = led.color.green as u8;
        color_buf[4] = led.color.blue as u8;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        io.write_report(&color_buf).await
    }

    async fn write_led_v2(&self, io: &mut DeviceIo, led: &crate::device::LedInfo) -> Result<()> {
        /* V2 LED report envelope (64 bytes):
         *   [0]      = report_id
         *   [1]      = padding
         *   [2]      = led_id
         *   [3..5]   = duration (u16 LE)
         *   [5..19]  = padding
         *   [19]     = disable_repeat
         *   [20..27] = padding
         *   [27]     = npoints
         *   [28..]   = points (4 bytes each: r, g, b, pos) */
        let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
        buf[0] = STEELSERIES_ID_LED;
        buf[2] = led.index as u8;

        if matches!(
            led.mode,
            crate::device::LedMode::Off | crate::device::LedMode::Solid
        ) {
            buf[19] = 0x01;
        }

        let mut npoints = 0usize;
        let c1 = &led.color;
        let off = led.mode == crate::device::LedMode::Off;

        /* Point 0 */
        let p = 28 + npoints * 4;
        buf[p] = if off { 0 } else { c1.red as u8 };
        buf[p + 1] = if off { 0 } else { c1.green as u8 };
        buf[p + 2] = if off { 0 } else { c1.blue as u8 };
        buf[p + 3] = 0x00;
        npoints += 1;

        if led.mode == crate::device::LedMode::Breathing {
            /* Point 1: full color at midpoint */
            let p = 28 + npoints * 4;
            buf[p] = c1.red as u8;
            buf[p + 1] = c1.green as u8;
            buf[p + 2] = c1.blue as u8;
            buf[p + 3] = 0x7F;
            npoints += 1;

            /* Point 2: black at midpoint */
            let p = 28 + npoints * 4;
            buf[p + 3] = 0x7F;
            npoints += 1;
        }

        buf[27] = npoints as u8;
        let d = std::cmp::max(npoints as u16 * 330, led.effect_duration as u16);
        buf[3..5].copy_from_slice(&d.to_le_bytes());

        io.write_report(&buf).await
    }

    async fn write_led_v3(&self, io: &mut DeviceIo, led: &crate::device::LedInfo) -> Result<()> {
        /* V3 LED report envelope (64 bytes):
         *   [0]      = report_id
         *   [1]      = padding
         *   [2]      = led_id
         *   [3..7]   = padding (4 bytes)
         *   [7]      = led_id2
         *   [8..10]  = duration (u16 LE)
         *   [10..24] = padding (14 bytes)
         *   [24]     = disable_repeat
         *   [25..29] = padding (4 bytes)
         *   [29]     = npoints
         *   [30..]   = points (4 bytes each: r, g, b, pos), max 8
         *   [62..64] = padding (2 bytes) */
        let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
        buf[0] = STEELSERIES_ID_LED_PROTOCOL3;
        buf[2] = led.index as u8;
        buf[7] = led.index as u8;

        if matches!(
            led.mode,
            crate::device::LedMode::Off | crate::device::LedMode::Solid
        ) {
            buf[24] = 0x01;
        }

        let mut npoints = 0usize;
        let c1 = &led.color;
        let off = led.mode == crate::device::LedMode::Off;

        /* Point 0 */
        let p = 30 + npoints * 4;
        buf[p] = if off { 0 } else { c1.red as u8 };
        buf[p + 1] = if off { 0 } else { c1.green as u8 };
        buf[p + 2] = if off { 0 } else { c1.blue as u8 };
        buf[p + 3] = 0x00;
        npoints += 1;

        if led.mode == crate::device::LedMode::Breathing {
            /* Point 1 */
            let p = 30 + npoints * 4;
            buf[p] = c1.red as u8;
            buf[p + 1] = c1.green as u8;
            buf[p + 2] = c1.blue as u8;
            buf[p + 3] = 0x7F;
            npoints += 1;

            /* Point 2 */
            let p = 30 + npoints * 4;
            buf[p + 3] = 0x7F;
            npoints += 1;
        }

        buf[29] = npoints as u8;
        let d = std::cmp::max(npoints as u16 * 330, led.effect_duration as u16);
        buf[8..10].copy_from_slice(&d.to_le_bytes());

        io.set_feature_report(&buf)?;
        Ok(())
    }

    async fn write_save(&self, io: &mut DeviceIo) -> Result<()> {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        match self.version {
            1 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE_SHORT];
                buf[0] = STEELSERIES_ID_SAVE_SHORT;
                io.write_report(&buf).await
            }
            2 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
                buf[0] = STEELSERIES_ID_SAVE;
                io.write_report(&buf).await
            }
            3 | 4 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
                buf[0] = STEELSERIES_ID_SAVE_PROTOCOL3;
                io.write_report(&buf).await
            }
            _ => Ok(()),
        }
    }

    async fn read_firmware_version(&self, io: &mut DeviceIo) -> Result<String> {
        match self.version {
            1 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE_SHORT];
                buf[0] = STEELSERIES_ID_FIRMWARE_PROTOCOL1;
                io.write_report(&buf).await?;
            }
            2 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
                buf[0] = STEELSERIES_ID_FIRMWARE_PROTOCOL2;
                io.write_report(&buf).await?;
            }
            3 => {
                let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
                buf[0] = STEELSERIES_ID_FIRMWARE_PROTOCOL3;
                io.write_report(&buf).await?;
            }
            _ => return Ok(String::new()),
        }

        /* Timeout to gracefully skip if the device doesn't respond (some variants are Write-Only) */
        let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
        if let Ok(Ok(n)) = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            io.read_report(&mut buf),
        )
        .await
        {
            if n >= 2 {
                /* Return formats as 'major.minor' - bound checking buffer size explicitly */
                let major = buf.get(1).copied().unwrap_or(0);
                let minor = buf.get(0).copied().unwrap_or(0);
                return Ok(format!("{}.{}", major, minor));
            }
        }

        Ok(String::new())
    }

    async fn read_settings(
        &self,
        io: &mut DeviceIo,
        profile: &mut crate::device::ProfileInfo,
    ) -> Result<()> {
        let settings_id = match self.version {
            2 => STEELSERIES_ID_SETTINGS,
            3 => STEELSERIES_ID_SETTINGS_PROTOCOL3,
            _ => return Ok(()),
        };

        let mut req = [0u8; STEELSERIES_REPORT_SIZE];
        req[0] = settings_id;
        io.write_report(&req).await?;

        let mut buf = [0u8; STEELSERIES_REPORT_SIZE];
        if let Ok(Ok(n)) = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            io.read_report(&mut buf),
        )
        .await
        {
            if n < 2 {
                return Ok(());
            }

            if self.version == 2 {
                let active_resolution = buf.get(1).copied().unwrap_or(0).saturating_sub(1);
                for res in &mut profile.resolutions {
                    res.is_active = res.index == active_resolution as u32;
                    let dpi_idx = 2 + res.index as usize * 2;
                    if dpi_idx < n {
                        let dpi_val = 100 * (1 + buf.get(dpi_idx).copied().unwrap_or(0) as u32);
                        res.dpi = crate::device::Dpi::Unified(dpi_val);
                    }
                }

                for led in &mut profile.leds {
                    let offset = 6 + led.index as usize * 3;
                    if offset + 2 < n {
                        led.color.red = buf.get(offset).copied().unwrap_or(0) as u32;
                        led.color.green = buf.get(offset + 1).copied().unwrap_or(0) as u32;
                        led.color.blue = buf.get(offset + 2).copied().unwrap_or(0) as u32;
                    }
                }
            } else if self.version == 3 {
                let active_resolution = buf.get(0).copied().unwrap_or(0).saturating_sub(1);
                for res in &mut profile.resolutions {
                    res.is_active = res.index == active_resolution as u32;
                }
            }
        }

        Ok(())
    }
}
