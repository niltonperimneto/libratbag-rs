use crate::device::DeviceInfo;
use crate::driver::{DeviceDriver, DeviceIo};
use anyhow::Result;
use async_trait::async_trait;
use tracing::debug;

const LOGITECH_G300_PROFILE_MAX: u32 = 2;
const LOGITECH_G300_BUTTON_MAX: u32 = 8;
const LOGITECH_G300_NUM_DPI: u32 = 4;

const LOGITECH_G300_REPORT_ID_GET_ACTIVE: u8 = 0xF0;
const LOGITECH_G300_REPORT_ID_PROFILE_0: u8 = 0xF3;
const LOGITECH_G300_REPORT_ID_PROFILE_1: u8 = 0xF4;
const LOGITECH_G300_REPORT_ID_PROFILE_2: u8 = 0xF5;

const LOGITECH_G300_REPORT_SIZE_ACTIVE: usize = 4;
const LOGITECH_G300_REPORT_SIZE_PROFILE: usize = 35;

#[derive(Clone, Copy, Default)]
pub struct LogitechG300Resolution {
    pub bitfield: u8, /* dpi (7-bit), is_default(1-bit) */
}

#[derive(Clone, Copy, Default)]
pub struct LogitechG300Button {
    pub code: u8,
    pub modifier: u8,
    pub key: u8,
}

#[derive(Clone, Copy)]
pub struct LogitechG300ProfileReport {
    pub id: u8,
    pub bitfield_led: u8, /* led_red (1) led_green (1) led_blue(1) unknown1(5) */
    pub frequency: u8,
    pub dpi_levels: [LogitechG300Resolution; 4],
    pub unknown2: u8,
    pub buttons: [LogitechG300Button; 9],
}

impl LogitechG300ProfileReport {
    pub fn new() -> Self {
        Self {
            id: 0,
            bitfield_led: 0,
            frequency: 0,
            dpi_levels: [LogitechG300Resolution::default(); 4],
            unknown2: 0,
            buttons: [LogitechG300Button::default(); 9],
        }
    }

    pub fn into_bytes(self) -> [u8; LOGITECH_G300_REPORT_SIZE_PROFILE] {
        let mut b = [0u8; LOGITECH_G300_REPORT_SIZE_PROFILE];
        b[0] = self.id;
        b[1] = self.bitfield_led;
        b[2] = self.frequency;
        for i in 0..4 {
            b[3 + i] = self.dpi_levels[i].bitfield;
        }
        b[7] = self.unknown2;
        let mut offset = 8;
        for btn in &self.buttons {
            b[offset] = btn.code;
            b[offset + 1] = btn.modifier;
            b[offset + 2] = btn.key;
            offset += 3;
        }
        b
    }

    pub fn from_bytes(b: &[u8; LOGITECH_G300_REPORT_SIZE_PROFILE]) -> Self {
        let mut s = Self::new();
        s.id = b[0];
        s.bitfield_led = b[1];
        s.frequency = b[2];
        for i in 0..4 {
            s.dpi_levels[i].bitfield = b[3 + i];
        }
        s.unknown2 = b[7];
        let mut offset = 8;
        for btn in &mut s.buttons {
            btn.code = b[offset];
            btn.modifier = b[offset + 1];
            btn.key = b[offset + 2];
            offset += 3;
        }
        s
    }
}

pub struct LogitechG300Driver {}

impl LogitechG300Driver {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl DeviceDriver for LogitechG300Driver {
    fn name(&self) -> &str {
        "Logitech G300"
    }

    async fn probe(&mut self, _io: &mut DeviceIo) -> Result<()> {
        debug!("Probe called for Logitech G300");
        Ok(())
    }

    async fn load_profiles(&mut self, io: &mut DeviceIo, info: &mut DeviceInfo) -> Result<()> {
        info.profiles.clear();

        /* Attempt to read Active Configuration to map indices before population */
        let mut active_idx = 0;
        let mut active_res = 0;
        let req = [LOGITECH_G300_REPORT_ID_GET_ACTIVE, 0, 0, 0];
        if let Ok(_) = io.write_report(&req).await {
            let mut buf = [0u8; LOGITECH_G300_REPORT_SIZE_ACTIVE];
            if let Ok(Ok(n)) = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                io.read_report(&mut buf),
            )
            .await
            {
                if n == 4 {
                    active_idx = buf[3] & 0x0F;
                    active_res = buf[2] & 0x07;
                }
            }
        }

        for profile_id in 0..=LOGITECH_G300_PROFILE_MAX {
            let mut profile = crate::device::ProfileInfo {
                index: profile_id,
                name: format!("Profile {}", profile_id),
                is_active: profile_id == active_idx as u32,
                is_enabled: true,
                is_dirty: false,
                resolutions: Vec::new(),
                buttons: Vec::new(),
                leds: Vec::new(),
                report_rate: 1000,
                report_rates: vec![125, 250, 500, 1000],
                angle_snapping: -1,
                debounce: -1,
                debounces: Vec::new(),
            };

            for res_id in 0..LOGITECH_G300_NUM_DPI {
                profile.resolutions.push(crate::device::ResolutionInfo {
                    index: res_id,
                    is_active: profile_id == active_idx as u32 && res_id == active_res as u32,
                    is_default: false,
                    is_disabled: false,
                    dpi: crate::device::Dpi::Unknown,
                    dpi_list: vec![],
                    capabilities: Vec::new(),
                });
            }

            for btn_id in 0..=LOGITECH_G300_BUTTON_MAX {
                profile.buttons.push(crate::device::ButtonInfo {
                    index: btn_id,
                    action_type: crate::device::ActionType::Unknown,
                    action_types: vec![0, 1, 2, 3, 4],
                    mapping_value: 0,
                    macro_entries: Vec::new(),
                });
            }

            profile.leds.push(crate::device::LedInfo {
                index: 0,
                mode: crate::device::LedMode::Solid,
                modes: vec![],
                color: crate::device::Color::default(),
                secondary_color: crate::device::Color::default(),
                tertiary_color: crate::device::Color::default(),
                color_depth: 1,
                effect_duration: 0,
                brightness: 255,
            });

            info.profiles.push(profile);
        }

        Ok(())
    }

    async fn commit(&mut self, io: &mut DeviceIo, info: &DeviceInfo) -> Result<()> {
        for profile in &info.profiles {
            if !profile.is_dirty {
                continue;
            }
            let mut report = LogitechG300ProfileReport::new();

            report.id = match profile.index {
                0 => LOGITECH_G300_REPORT_ID_PROFILE_0,
                1 => LOGITECH_G300_REPORT_ID_PROFILE_1,
                2 => LOGITECH_G300_REPORT_ID_PROFILE_2,
                _ => continue,
            };

            /* Convert Hz */
            report.frequency = match profile.report_rate {
                1000 => 0,
                125 => 1,
                250 => 2,
                500 => 3,
                _ => 0,
            };

            for btn in &profile.buttons {
                let btn_idx = btn.index as usize;
                if btn_idx >= report.buttons.len() {
                    tracing::warn!("G300: button index {} out of range (max {}), skipping",
                                   btn.index, report.buttons.len() - 1);
                    continue;
                }
                let mut data = LogitechG300Button::default();
                match btn.action_type {
                    crate::device::ActionType::Button => {
                        let val = btn.mapping_value as u8;
                        if (1..=9).contains(&val) {
                            data.code = val;
                        }
                    }
                    crate::device::ActionType::Special => {
                        match btn.mapping_value {
                            2 => data.code = 0x0A, // RES_UP
                            3 => data.code = 0x0B, // RES_DOWN
                            _ => data.code = 0x0C, // Generic Special
                        }
                    }
                    crate::device::ActionType::Key | crate::device::ActionType::Macro => {
                        data.code = 0x00;
                        /* Write simplified key mapping */
                        data.key = (btn.mapping_value % 256) as u8;
                        data.modifier = 0x00;
                    }
                    _ => {}
                }
                report.buttons[btn_idx] = data;
            }

            if let Some(led) = profile.leds.first() {
                let r = if led.color.red > 127 { 0x01 } else { 0x00 };
                let g = if led.color.green > 127 { 0x02 } else { 0x00 };
                let b = if led.color.blue > 127 { 0x04 } else { 0x00 };
                report.bitfield_led = r | g | b;
            }

            let b = report.into_bytes();
            io.write_report(&b).await?;
        }
        Ok(())
    }
}
