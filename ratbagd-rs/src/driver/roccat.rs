use crate::device::DeviceInfo;
use crate::driver::{DeviceDriver, DeviceIo, DriverError};
use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::debug;
use std::time::Duration;

/* Protocol constants from driver-roccat.c */
#[allow(dead_code)]
const ROCCAT_PROFILE_MAX: u8 = 4;
#[allow(dead_code)]
const ROCCAT_BUTTON_MAX: u8 = 23;
#[allow(dead_code)]
const ROCCAT_NUM_DPI: u8 = 5;

const ROCCAT_REPORT_ID_CONFIGURE_PROFILE: u8 = 4;
const ROCCAT_REPORT_ID_PROFILE: u8 = 5;
#[allow(dead_code)]
const ROCCAT_REPORT_ID_SETTINGS: u8 = 6;
#[allow(dead_code)]
const ROCCAT_REPORT_ID_KEY_MAPPING: u8 = 7;
#[allow(dead_code)]
const ROCCAT_REPORT_ID_MACRO: u8 = 8;

const ROCCAT_MAX_RETRY_READY: usize = 10;
#[allow(dead_code)]
const ROCCAT_MAX_MACRO_LENGTH: usize = 500;

/* Each Roccat button mapping is a 3-byte stride: [action, param1, param2] */
const ROCCAT_BUTTON_STRIDE: usize = 3;
/* Maximum button index (0-based). 24 buttons × 3 bytes = 72 = buttons array len */
const ROCCAT_BUTTON_INDEX_MAX: usize = 24;

#[derive(Debug, Clone, Copy)]
pub struct RoccatSettingsReport {
    pub report_id: u8,
    pub report_length: u8,
    pub profile_id: u8,
    pub x_y_linked: u8,
    pub x_sensitivity: u8,
    pub y_sensitivity: u8,
    pub dpi_mask: u8,
    pub xres: [u8; 5],
    pub current_dpi: u8,
    pub yres: [u8; 5],
    pub padding1: u8,
    pub report_rate: u8,
    pub padding2: [u8; 21],
    pub checksum: u16,
}

impl RoccatSettingsReport {
    pub fn from_bytes(buf: &[u8; 43]) -> Self {
        let mut xres = [0u8; 5];
        xres.copy_from_slice(&buf[7..12]);
        let mut yres = [0u8; 5];
        yres.copy_from_slice(&buf[13..18]);
        let mut padding2 = [0u8; 21];
        padding2.copy_from_slice(&buf[20..41]);
        
        Self {
            report_id: buf[0],
            report_length: buf[1],
            profile_id: buf[2],
            x_y_linked: buf[3],
            x_sensitivity: buf[4],
            y_sensitivity: buf[5],
            dpi_mask: buf[6],
            xres,
            current_dpi: buf[12],
            yres,
            padding1: buf[18],
            report_rate: buf[19],
            padding2,
            checksum: u16::from_le_bytes([buf[41], buf[42]]),
        }
    }

    pub fn into_bytes(self) -> [u8; 43] {
        let mut buf = [0u8; 43];
        buf[0] = self.report_id;
        buf[1] = self.report_length;
        buf[2] = self.profile_id;
        buf[3] = self.x_y_linked;
        buf[4] = self.x_sensitivity;
        buf[5] = self.y_sensitivity;
        buf[6] = self.dpi_mask;
        buf[7..12].copy_from_slice(&self.xres);
        buf[12] = self.current_dpi;
        buf[13..18].copy_from_slice(&self.yres);
        buf[18] = self.padding1;
        buf[19] = self.report_rate;
        buf[20..41].copy_from_slice(&self.padding2);
        buf[41..43].copy_from_slice(&self.checksum.to_le_bytes());
        buf
    }
}

#[derive(Clone, Copy)]
pub struct RoccatProfileReport {
    pub report_id: u8,
    pub report_length: u8,
    pub profile_id: u8,
    pub buttons: [u8; 72],
    pub checksum: u16,
}

impl RoccatProfileReport {
    pub fn from_bytes(buf: &[u8; 77]) -> Self {
        let mut buttons = [0u8; 72];
        buttons.copy_from_slice(&buf[3..75]);
        Self {
            report_id: buf[0],
            report_length: buf[1],
            profile_id: buf[2],
            buttons,
            checksum: u16::from_le_bytes([buf[75], buf[76]]),
        }
    }
    
    pub fn into_bytes(self) -> [u8; 77] {
        let mut buf = [0u8; 77];
        buf[0] = self.report_id;
        buf[1] = self.report_length;
        buf[2] = self.profile_id;
        buf[3..75].copy_from_slice(&self.buttons);
        buf[75..77].copy_from_slice(&self.checksum.to_le_bytes());
        buf
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct RoccatMacroEvent {
    pub keycode: u8,
    pub flag: u8,
    pub time: u16,
}

#[allow(dead_code)]
#[derive(Clone, Copy)]
pub struct RoccatMacro {
    pub report_id: u8,
    pub report_length: u16,
    pub profile: u8,
    pub button_index: u8,
    pub active: u8,
    pub padding: [u8; 24],
    pub group: [u8; 24],
    pub name: [u8; 24],
    pub length: u16,
    pub keys: [RoccatMacroEvent; ROCCAT_MAX_MACRO_LENGTH],
    pub checksum: u16,
}

impl RoccatMacro {
    pub fn from_bytes(buf: &[u8; 2082]) -> Self {
        let mut padding = [0u8; 24];
        padding.copy_from_slice(&buf[6..30]);
        let mut group = [0u8; 24];
        group.copy_from_slice(&buf[30..54]);
        let mut name = [0u8; 24];
        name.copy_from_slice(&buf[54..78]);
        
        let mut keys = [RoccatMacroEvent { keycode: 0, flag: 0, time: 0 }; ROCCAT_MAX_MACRO_LENGTH];
        for i in 0..ROCCAT_MAX_MACRO_LENGTH {
            let offset = 80 + i * 4;
            keys[i] = RoccatMacroEvent {
                keycode: buf[offset],
                flag: buf[offset + 1],
                time: u16::from_le_bytes([buf[offset + 2], buf[offset + 3]]),
            };
        }
        
        Self {
            report_id: buf[0],
            report_length: u16::from_le_bytes([buf[1], buf[2]]),
            profile: buf[3],
            button_index: buf[4],
            active: buf[5],
            padding,
            group,
            name,
            length: u16::from_le_bytes([buf[78], buf[79]]),
            keys,
            checksum: u16::from_le_bytes([buf[2080], buf[2081]]),
        }
    }
    
    pub fn into_bytes(self) -> [u8; 2082] {
        let mut buf = [0u8; 2082];
        buf[0] = self.report_id;
        buf[1..3].copy_from_slice(&self.report_length.to_le_bytes());
        buf[3] = self.profile;
        buf[4] = self.button_index;
        buf[5] = self.active;
        buf[6..30].copy_from_slice(&self.padding);
        buf[30..54].copy_from_slice(&self.group);
        buf[54..78].copy_from_slice(&self.name);
        buf[78..80].copy_from_slice(&self.length.to_le_bytes());
        
        for (i, key) in self.keys.iter().enumerate() {
            let offset = 80 + i * 4;
            buf[offset] = key.keycode;
            buf[offset + 1] = key.flag;
            buf[offset + 2..offset + 4].copy_from_slice(&key.time.to_le_bytes());
        }
        
        buf[2080..2082].copy_from_slice(&self.checksum.to_le_bytes());
        buf
    }
}

pub struct RoccatDriver {
    name: String,
    /* Cache of the latest settings report per profile, updated during */
    /* load_profiles and modified during commit.                       */
    cached_settings: [Option<RoccatSettingsReport>; (ROCCAT_PROFILE_MAX + 1) as usize],
    /* Cache of the latest key mapping report per profile. */
    cached_profiles: [Option<RoccatProfileReport>; (ROCCAT_PROFILE_MAX + 1) as usize],
}

/* Translate a raw Roccat bytecode to a unified (ActionType, mapping_value). */
#[allow(dead_code)]
fn roccat_raw_to_action(raw: u8) -> (crate::device::ActionType, u32) {
    use crate::device::ActionType;
    match raw {
        1 => (ActionType::Button, 1),
        2 => (ActionType::Button, 2),
        3 => (ActionType::Button, 3),
        4 => (ActionType::Special, 1), // DOUBLECLICK
        6 => (ActionType::None, 0),
        7 => (ActionType::Button, 4),
        8 => (ActionType::Button, 5),
        9 => (ActionType::Special, 2), // WHEEL_LEFT
        10 => (ActionType::Special, 3), // WHEEL_RIGHT
        13 => (ActionType::Special, 4), // WHEEL_UP
        14 => (ActionType::Special, 5), // WHEEL_DOWN
        16 => (ActionType::Special, 6), // PROFILE_CYCLE_UP
        17 => (ActionType::Special, 7), // PROFILE_UP
        18 => (ActionType::Special, 8), // PROFILE_DOWN
        20 => (ActionType::Special, 9), // RESOLUTION_CYCLE_UP
        21 => (ActionType::Special, 10), // RESOLUTION_UP
        22 => (ActionType::Special, 11), // RESOLUTION_DOWN
        26 => (ActionType::Key, 125), // KEY_LEFTMETA
        32 => (ActionType::Key, 171), // KEY_CONFIG
        33 => (ActionType::Key, 163), // KEY_PREVIOUSSONG
        34 => (ActionType::Key, 165), // KEY_NEXTSONG
        35 => (ActionType::Key, 164), // KEY_PLAYPAUSE
        36 => (ActionType::Key, 166), // KEY_STOPCD
        37 => (ActionType::Key, 113), // KEY_MUTE
        38 => (ActionType::Key, 115), // KEY_VOLUMEUP
        39 => (ActionType::Key, 114), // KEY_VOLUMEDOWN
        48 => (ActionType::Macro, 0),
        65 => (ActionType::Special, 20), // SECOND_MODE
        _ => (ActionType::Unknown, raw as u32),
    }
}

/* Translate a unified (ActionType, mapping_value) back to a raw Roccat bytecode. */
#[allow(dead_code)]
fn roccat_action_to_raw(action: crate::device::ActionType, val: u32) -> u8 {
    use crate::device::ActionType;
    match (action, val) {
        (ActionType::Button, 1) => 1,
        (ActionType::Button, 2) => 2,
        (ActionType::Button, 3) => 3,
        (ActionType::Button, 4) => 7,
        (ActionType::Button, 5) => 8,
        (ActionType::Special, 1) => 4,
        (ActionType::Special, 2) => 9,
        (ActionType::Special, 3) => 10,
        (ActionType::Special, 4) => 13,
        (ActionType::Special, 5) => 14,
        (ActionType::Special, 6) => 16,
        (ActionType::Special, 7) => 17,
        (ActionType::Special, 8) => 18,
        (ActionType::Special, 9) => 20,
        (ActionType::Special, 10) => 21,
        (ActionType::Special, 11) => 22,
        (ActionType::Key, 125) => 26,
        (ActionType::Key, 171) => 32,
        (ActionType::Key, 163) => 33,
        (ActionType::Key, 165) => 34,
        (ActionType::Key, 164) => 35,
        (ActionType::Key, 166) => 36,
        (ActionType::Key, 113) => 37,
        (ActionType::Key, 115) => 38,
        (ActionType::Key, 114) => 39,
        (ActionType::Macro, _) => 48,
        (ActionType::Special, 20) => 65,
        (ActionType::None, _) => 6,
        (ActionType::Unknown, raw) => raw as u8,
        _ => 6, // Fallback to None
    }
}

impl RoccatDriver {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            cached_settings: [None; 5],
            cached_profiles: [None; 5],
        }
    }

    /* Asynchronous translation of `roccat_wait_ready` from driver-roccat.c. */
    /*                                                                        */
    /* The C implementation blocks on `msleep(10)` in a tight loop. In the   */
    /* Tokio actor model, blocking the thread is a fatal error. This version  */
    /* yields to the executor between each poll so other devices remain live. */
    async fn wait_ready(&self, io: &mut DeviceIo) -> Result<()> {
        let mut count = 0;
        let mut backoff_ms: u64 = 10;

        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;

        while count < ROCCAT_MAX_RETRY_READY {
            let mut buf = [0u8; 3];
            buf[0] = ROCCAT_REPORT_ID_CONFIGURE_PROFILE;

            if let Ok(len) = io.get_feature_report(&mut buf)
                && len == 3
            {
                match buf[1] {
                    0x01 => return Ok(()),
                    0x02 => {
                        /* C returns rc=2 for this state; callers treat */
                        /* it as a non-fatal error that aborts the op. */
                        return Err(anyhow::anyhow!(
                            "Roccat device reported error state (0x02)"
                        ));
                    }
                    0x03 => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    _ => { /* unknown state, retry */ }
                }
            }

            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(100);
            count += 1;
        }

        Err(DriverError::Timeout { attempts: ROCCAT_MAX_RETRY_READY as u8 }.into())
    }

    /* Purely functional CRC computation from `roccat_compute_crc` in driver-roccat.c. */
    /*                                                                                 */
    /* The CRC is a simple wrapping sum of all bytes except the trailing two.          */
    /* The original C function mutated a local accumulator; this version is pure.      */
    #[allow(dead_code)]
    fn compute_crc(buf: &[u8]) -> u16 {
        if buf.len() < 3 {
            return 0;
        }

        buf[0..buf.len() - 2]
            .iter()
            .fold(0u16, |acc, &b| acc.wrapping_add(b as u16))
    }

    /* Validate the CRC embedded in the last two bytes of `buf` (little-endian). */
    #[allow(dead_code)]
    fn crc_is_valid(buf: &[u8]) -> bool {
        if buf.len() < 3 {
            return false;
        }

        let computed = Self::compute_crc(buf);
        let received = u16::from_le_bytes([buf[buf.len() - 2], buf[buf.len() - 1]]);

        computed == received
    }

    /* Configure the device to expose the given profile and type on its interface. */
    async fn set_config_profile(&self, io: &mut DeviceIo, profile_idx: u8, config_type: u8) -> Result<()> {
        if profile_idx > ROCCAT_PROFILE_MAX {
            return Err(anyhow::anyhow!("Profile index {} out of bounds", profile_idx));
        }

        let buf = [ROCCAT_REPORT_ID_CONFIGURE_PROFILE, profile_idx, config_type];
        io.set_feature_report(&buf)?;
        self.wait_ready(io).await.context("Failed wait_ready after set_config_profile")?;
        Ok(())
    }

    /* Read the settings report for a specific profile securely validating CRC. */
    async fn read_settings(&self, io: &mut DeviceIo, profile_idx: u8) -> Result<RoccatSettingsReport> {
        const ROCCAT_CONFIG_SETTINGS: u8 = 0x80;
        self.set_config_profile(io, profile_idx, ROCCAT_CONFIG_SETTINGS).await?;

        let mut buf = [0u8; 43];
        buf[0] = ROCCAT_REPORT_ID_SETTINGS;
        
        let len = io.get_feature_report(&mut buf).context("Failed to get settings report")?;
        if len < 43 {
            return Err(DriverError::BufferTooSmall {
                expected: 43,
                actual: len,
            }.into());
        }

        if !Self::crc_is_valid(&buf) {
            let computed = Self::compute_crc(&buf);
            let received = u16::from_le_bytes([buf[41], buf[42]]);
            return Err(DriverError::ChecksumMismatch { computed, received }.into());
        }

        Ok(RoccatSettingsReport::from_bytes(&buf))
    }

    /* Read the key mapping profile report securely validating CRC. */
    async fn read_profile_report(&self, io: &mut DeviceIo, profile_idx: u8) -> Result<RoccatProfileReport> {
        const ROCCAT_CONFIG_KEY_MAPPING: u8 = 0x90;
        const ROCCAT_REPORT_ID_KEY_MAPPING: u8 = 7;
        self.set_config_profile(io, profile_idx, ROCCAT_CONFIG_KEY_MAPPING).await?;

        let mut buf = [0u8; 77];
        buf[0] = ROCCAT_REPORT_ID_KEY_MAPPING;

        /* Give device time to switch to the profile payload */
        tokio::time::sleep(Duration::from_millis(10)).await;

        let len = io.get_feature_report(&mut buf).context("Failed to get profile mapping report")?;
        if len < 77 {
            return Err(DriverError::BufferTooSmall {
                expected: 77,
                actual: len,
            }.into());
        }

        if !Self::crc_is_valid(&buf) {
            let computed = Self::compute_crc(&buf);
            let received = u16::from_le_bytes([buf[75], buf[76]]);
            return Err(DriverError::ChecksumMismatch { computed, received }.into());
        }

        Ok(RoccatProfileReport::from_bytes(&buf))
    }

    /* Write the settings report back to the device securely writing CRC. */
    async fn write_settings(&self, io: &mut DeviceIo, report: &mut RoccatSettingsReport) -> Result<()> {
        let mut buf = (*report).into_bytes();
        let crc = Self::compute_crc(&buf);
        report.checksum = crc; /* Update the struct in memory too */
        
        /* Serialize the CRC into the last two bytes (little-endian) */
        let crc_bytes = crc.to_le_bytes();
        buf[41] = crc_bytes[0];
        buf[42] = crc_bytes[1];

        io.set_feature_report(&buf).context("Failed to set settings report")?;
        self.wait_ready(io).await.context("Failed wait_ready after writing settings")?;
        Ok(())
    }

    /* Write the key mapping profile report back to the device securely writing CRC. */
    async fn write_profile_report(&self, io: &mut DeviceIo, profile_idx: u8, report: &mut RoccatProfileReport) -> Result<()> {
        const ROCCAT_CONFIG_KEY_MAPPING: u8 = 0x90;
        self.set_config_profile(io, profile_idx, ROCCAT_CONFIG_KEY_MAPPING).await?;

        let mut buf = (*report).into_bytes();
        let crc = Self::compute_crc(&buf);
        report.checksum = crc;

        let crc_bytes = crc.to_le_bytes();
        buf[75] = crc_bytes[0];
        buf[76] = crc_bytes[1];

        io.set_feature_report(&buf).context("Failed to set profile mapping report")?;
        self.wait_ready(io).await.context("Failed wait_ready after writing profile mapping")?;
        Ok(())
    }

    #[allow(dead_code)]
    async fn read_macro(&self, io: &mut DeviceIo, profile_idx: u8, btn_idx: u8) -> Result<RoccatMacro> {
        self.set_config_profile(io, profile_idx, 0).await?;
        self.set_config_profile(io, profile_idx, btn_idx).await?;

        let mut buf = [0u8; 2082];
        buf[0] = ROCCAT_REPORT_ID_MACRO;
        
        tokio::time::sleep(Duration::from_millis(10)).await;

        let len = io.get_feature_report(&mut buf).context("Failed to get macro report")?;
        if len < 2082 {
            return Err(DriverError::BufferTooSmall { expected: 2082, actual: len }.into());
        }

        if !Self::crc_is_valid(&buf) {
            let computed = Self::compute_crc(&buf);
            let received = u16::from_le_bytes([buf[2080], buf[2081]]);
            return Err(DriverError::ChecksumMismatch { computed, received }.into());
        }

        Ok(RoccatMacro::from_bytes(&buf))
    }

    #[allow(dead_code)]
    async fn write_macro(&self, io: &mut DeviceIo, report: &mut RoccatMacro) -> Result<()> {
        let mut buf = (*report).into_bytes();
        let crc = Self::compute_crc(&buf);
        report.checksum = crc;

        let crc_bytes = crc.to_le_bytes();
        buf[2080] = crc_bytes[0];
        buf[2081] = crc_bytes[1];

        io.set_feature_report(&buf).context("Failed to set macro report")?;
        self.wait_ready(io).await.context("Failed wait_ready after writing macro")?;
        Ok(())
    }
}

#[async_trait]
impl DeviceDriver for RoccatDriver {
    fn name(&self) -> &str {
        &self.name
    }

    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()> {
        let mut buf = [0u8; 3];
        buf[0] = ROCCAT_REPORT_ID_PROFILE;
        let len = io.get_feature_report(&mut buf)?;

        if len != 3 {
            return Err(anyhow::anyhow!(
                "Roccat probe failed: expected 3-byte feature report, got {len}"
            ));
        }

        debug!("Roccat device probed. Current profile: {}", buf[2]);
        Ok(())
    }

    async fn load_profiles(&mut self, io: &mut DeviceIo, info: &mut DeviceInfo) -> Result<()> {
        for profile_idx in 0..=ROCCAT_PROFILE_MAX {
            match self.read_settings(io, profile_idx).await {
                Ok(settings) => {
                    self.cached_settings[profile_idx as usize] = Some(settings);

                    if let Some(profile) = info.profiles.iter_mut().find(|p| p.index == profile_idx as u32) {
                        for res_idx in 0..ROCCAT_NUM_DPI {
                            let xres = settings.xres[res_idx as usize];
                            let yres = settings.yres[res_idx as usize];
                            let is_active = settings.current_dpi == res_idx;
                            let is_enabled = (settings.dpi_mask & (1 << res_idx)) != 0;

                            let dpi_x = if is_enabled { xres as u32 * 50 } else { 0 };
                            let dpi_y = if is_enabled { yres as u32 * 50 } else { 0 };

                            if let Some(res) = profile.resolutions.iter_mut().find(|r| r.index == res_idx as u32) {
                                res.is_active = is_active;
                                res.dpi = crate::device::Dpi::Separate { x: dpi_x, y: dpi_y };
                            }
                        }

                        let rates = [125, 250, 500, 1000];
                        if let Some(&rate) = rates.get(settings.report_rate as usize) {
                            profile.report_rate = rate;
                            profile.report_rates = rates.to_vec();
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Roccat: failed to read settings for profile {}: {}", profile_idx, e);
                }
            }

            match self.read_profile_report(io, profile_idx).await {
                Ok(profile_report) => {
                    self.cached_profiles[profile_idx as usize] = Some(profile_report);

                    if let Some(profile_info) = info.profiles.iter_mut().find(|p| p.index == profile_idx as u32) {
                        for button_info in &mut profile_info.buttons {
                            let btn_idx = button_info.index as usize;
                            if btn_idx < ROCCAT_BUTTON_INDEX_MAX {
                                debug_assert!(btn_idx * ROCCAT_BUTTON_STRIDE < profile_report.buttons.len());
                                let raw_action = profile_report.buttons[btn_idx * ROCCAT_BUTTON_STRIDE];
                                let (action_type, mapping_val) = roccat_raw_to_action(raw_action);
                                button_info.action_type = action_type;
                                button_info.mapping_value = mapping_val;
                                
                                if action_type == crate::device::ActionType::Macro {
                                    match self.read_macro(io, profile_idx, btn_idx as u8).await {
                                        Ok(macro_rep) => {
                                            let mut entries = Vec::new();
                                            for j in 0..macro_rep.length as usize {
                                                if j >= ROCCAT_MAX_MACRO_LENGTH { break; }
                                                let ev = macro_rep.keys[j];
                                                // Using ratbag conventions: 0=Press, 1=Release, 2=Wait
                                                if ev.flag & 0x01 != 0 {
                                                    entries.push((0, ev.keycode as u32));
                                                } else if ev.flag & 0x02 != 0 {
                                                    entries.push((1, ev.keycode as u32));
                                                }
                                                // Every key event has an associated wait time
                                                let time = if ev.time > 0 { ev.time } else { 50 };
                                                entries.push((2, time as u32));
                                            }
                                            button_info.macro_entries = entries;
                                        }
                                        Err(e) => tracing::warn!("Roccat: failed to read macro for btn {}: {}", btn_idx, e),
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Roccat: failed to read key mapping for profile {}: {}", profile_idx, e);
                }
            }
        }
        Ok(())
    }

    async fn commit(&mut self, io: &mut DeviceIo, info: &DeviceInfo) -> Result<()> {
        /* Write profile settings (DPI, polling rate) and key mappings (Buttons) */
        for profile in &info.profiles {
            let p_idx = profile.index as usize;
            if p_idx > ROCCAT_PROFILE_MAX as usize {
                continue;
            }

            if let Some(mut settings) = self.cached_settings[p_idx] {
                for res in &profile.resolutions {
                    let r_idx = res.index as usize;
                    if r_idx >= ROCCAT_NUM_DPI as usize { continue; }

                    match res.dpi {
                        crate::device::Dpi::Separate { x, y } => {
                            settings.xres[r_idx] = (x / 50) as u8;
                            settings.yres[r_idx] = (y / 50) as u8;
                        }
                        crate::device::Dpi::Unified(val) => {
                            settings.xres[r_idx] = (val / 50) as u8;
                            settings.yres[r_idx] = (val / 50) as u8;
                        }
                        crate::device::Dpi::Unknown => {}
                    }
                    if res.is_active {
                        settings.current_dpi = r_idx as u8;
                    }
                }

                let rates = [125, 250, 500, 1000];
                if let Some(idx) = rates.iter().position(|&r| r == profile.report_rate) {
                    settings.report_rate = idx as u8;
                }

                if let Err(e) = self.write_settings(io, &mut settings).await {
                    tracing::warn!("Roccat: failed to commit settings for profile {}: {}", profile.index, e);
                } else {
                    self.cached_settings[p_idx] = Some(settings);
                }
            }

            if let Some(mut profile_report) = self.cached_profiles[p_idx] {
                for button_info in &profile.buttons {
                    let btn_idx = button_info.index as usize;
                    if btn_idx < ROCCAT_BUTTON_INDEX_MAX {
                        debug_assert!(btn_idx * ROCCAT_BUTTON_STRIDE < profile_report.buttons.len());
                        let raw_action = roccat_action_to_raw(button_info.action_type, button_info.mapping_value);
                        profile_report.buttons[btn_idx * ROCCAT_BUTTON_STRIDE] = raw_action;

                        if button_info.action_type == crate::device::ActionType::Macro {
                            let mut macro_rep = RoccatMacro {
                                report_id: ROCCAT_REPORT_ID_MACRO,
                                report_length: 0x0822,
                                profile: profile.index as u8,
                                button_index: btn_idx as u8,
                                active: 0x01,
                                padding: [0; 24],
                                group: [0; 24],
                                name: [0; 24],
                                length: 0,
                                keys: [RoccatMacroEvent { keycode: 0, flag: 0, time: 0 }; ROCCAT_MAX_MACRO_LENGTH],
                                checksum: 0,
                            };
                            
                            // Initialize group and name with default values as C driver does
                            macro_rep.group[0] = b'g'; macro_rep.group[1] = b'0';
                            
                            let mut count = 0;
                            for (ev_type, val) in &button_info.macro_entries {
                                if count >= ROCCAT_MAX_MACRO_LENGTH { break; }
                                match *ev_type {
                                    0 => { 
                                        macro_rep.keys[count].flag = 0x01;
                                        macro_rep.keys[count].keycode = *val as u8;
                                        count += 1;
                                    }
                                    1 => {
                                        macro_rep.keys[count].flag = 0x02;
                                        macro_rep.keys[count].keycode = *val as u8;
                                        count += 1;
                                    }
                                    2 => {
                                        if count > 0 {
                                            macro_rep.keys[count - 1].time = *val as u16;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            macro_rep.length = count as u16;
                            
                            if let Err(e) = self.write_macro(io, &mut macro_rep).await {
                                tracing::warn!("Roccat: failed to write macro for btn {}: {}", btn_idx, e);
                            }
                        }
                    }
                }

                if let Err(e) = self.write_profile_report(io, profile.index as u8, &mut profile_report).await {
                    tracing::warn!("Roccat: failed to commit profile mapping for profile {}: {}", profile.index, e);
                } else {
                    self.cached_profiles[p_idx] = Some(profile_report);
                }
            }
        }

        /* Set active profile */
        if let Some(active_profile) = info.profiles.iter().find(|p| p.is_active) {
            let idx = active_profile.index as u8;
            if idx <= ROCCAT_PROFILE_MAX {
                let buf = [ROCCAT_REPORT_ID_PROFILE, 0x03, idx];
                io.set_feature_report(&buf).context("Failed to set active profile")?;
                self.wait_ready(io).await.context("Failed wait_ready after setting active profile")?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roccat_compute_crc_basic() {
        /* Bytes 0..3 sum = 0x01 + 0x02 + 0x03 = 0x06; bytes 4-5 are the CRC */
        let buf = [0x01, 0x02, 0x03, 0x06, 0x00];
        assert_eq!(RoccatDriver::compute_crc(&buf), 0x0006);
        assert!(RoccatDriver::crc_is_valid(&buf));
    }

    #[test]
    fn test_roccat_compute_crc_mismatched() {
        let buf = [0x01, 0x02, 0x03, 0xFF, 0x00];
        assert!(!RoccatDriver::crc_is_valid(&buf));
    }

    #[test]
    fn test_roccat_compute_crc_too_short() {
        assert_eq!(RoccatDriver::compute_crc(&[0x01, 0x02]), 0);
        assert!(!RoccatDriver::crc_is_valid(&[0x01, 0x02]));
    }

    #[test]
    fn test_roccat_compute_crc_wrapping() {
        /* All 0xFF bytes: 3 bytes, sum of first one is 0xFF, wraps cleanly */
        let crc = 0xFFu16;
        let buf = [0xFF, crc as u8, (crc >> 8) as u8];
        assert_eq!(RoccatDriver::compute_crc(&buf), crc);
        assert!(RoccatDriver::crc_is_valid(&buf));
    }
}
