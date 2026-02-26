/* Logitech HID++ 2.0 driver implementation. */
/*  */
/* HID++ 2.0 is the modern feature-based protocol used by most current */
/* Logitech gaming mice. Each capability is exposed as a numbered "feature" */
/* that must be discovered at probe time via the Root feature (0x0000). */

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::device::{Color, DeviceInfo, Dpi, LedMode, ProfileInfo, RgbColor};
use crate::driver::DeviceIo;

use super::hidpp::{
    self, HidppReport, DEVICE_IDX_WIRED, LED_HW_MODE_BREATHING, LED_HW_MODE_COLOR_WAVE,
    LED_HW_MODE_CYCLE, LED_HW_MODE_FIXED, LED_HW_MODE_OFF, LED_HW_MODE_STARLIGHT,
    LED_PAYLOAD_SIZE, PAGE_ADJUSTABLE_DPI, PAGE_ADJUSTABLE_REPORT_RATE,
    PAGE_COLOR_LED_EFFECTS, PAGE_DEVICE_NAME, PAGE_ONBOARD_PROFILES, PAGE_RGB_EFFECTS,
    PAGE_SPECIAL_KEYS_BUTTONS, ROOT_FEATURE_INDEX, ROOT_FN_GET_FEATURE,
    ROOT_FN_GET_PROTOCOL_VERSION,
};

/* Software ID used in all our requests (arbitrary, identifies us) */
const SW_ID: u8 = 0x04;

/* Adjustable DPI (0x2201) function IDs */
const DPI_FN_GET_SENSOR_COUNT: u8 = 0x00;
const DPI_FN_GET_SENSOR_DPI: u8 = 0x01;

/* Adjustable Report Rate (0x8060) function IDs */
const RATE_FN_GET_REPORT_RATE_LIST: u8 = 0x00;
const RATE_FN_GET_REPORT_RATE: u8 = 0x01;

/* Color LED Effects (0x8070) function IDs */
const LED_FN_GET_ZONE_EFFECT: u8 = 0x01;
const LED_FN_SET_ZONE_EFFECT: u8 = 0x02;

/* A feature page → runtime index mapping for a known set of capabilities. */
#[derive(Debug, Default)]
struct FeatureMap {
    adjustable_dpi: Option<u8>,
    special_keys: Option<u8>,
    onboard_profiles: Option<u8>,
    color_led_effects: Option<u8>,
    rgb_effects: Option<u8>,
    report_rate: Option<u8>,
    device_name: Option<u8>,
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
            PAGE_DEVICE_NAME => self.device_name = Some(index),
            _ => {}
        }
    }
}

/* Protocol version stored after a successful probe. */
#[derive(Debug, Clone, Copy, Default)]
struct ProtocolVersion {
    #[allow(dead_code)]
    major: u8,
    #[allow(dead_code)]
    minor: u8,
}

pub struct Hidpp20Driver {
    device_index: u8,
    version: ProtocolVersion,
    features: FeatureMap,
}

impl Hidpp20Driver {
    pub fn new() -> Self {
        Self {
            device_index: DEVICE_IDX_WIRED,
            version: ProtocolVersion::default(),
            features: FeatureMap::default(),
        }
    }

    /* Query the Root feature (0x0000, fn 0) to find the runtime index of */
    /* a given feature page. Returns `None` if the device does not support it. */
    async fn get_feature_index(
        &self,
        io: &mut DeviceIo,
        feature_page: u16,
    ) -> Result<Option<u8>> {
        let [hi, lo] = feature_page.to_be_bytes();

        let request = hidpp::build_hidpp20_request(
            self.device_index,
            ROOT_FEATURE_INDEX,
            ROOT_FN_GET_FEATURE,
            SW_ID,
            &[hi, lo],
        );

        let dev_idx = self.device_index;
        io.request(&request, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.is_error() {
                return Some(None);
            }
            if !report.matches_hidpp20(dev_idx, ROOT_FEATURE_INDEX) {
                return None;
            }
            if let HidppReport::Long { params, .. } = report {
                let index = params[0];
                Some(if index == 0 { None } else { Some(index) })
            } else {
                None
            }
        })
        .await
        .with_context(|| format!("Feature lookup for 0x{feature_page:04X} failed"))
    }

    /* Send a HID++ 2.0 feature request and return the 16-byte response payload. */
    async fn feature_request(
        &self,
        io: &mut DeviceIo,
        feature_index: u8,
        function: u8,
        params: &[u8],
    ) -> Result<[u8; 16]> {
        let request = hidpp::build_hidpp20_request(
            self.device_index,
            feature_index,
            function,
            SW_ID,
            params,
        );

        let dev_idx = self.device_index;
        io.request(&request, 3, move |buf| {
            let report = HidppReport::parse(buf)?;
            if report.matches_hidpp20(dev_idx, feature_index)
                && let HidppReport::Long { params, .. } = report
            {
                return Some(params);
            }
            None
        })
        .await
        .with_context(|| {
            format!("Feature request (idx=0x{feature_index:02X}, fn={function}) failed")
        })
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
            (PAGE_DEVICE_NAME, "Device Name"),
        ];

        for &(page, name) in FEATURE_QUERIES {
            match self.get_feature_index(io, page).await {
                Ok(Some(idx)) => {
                    debug!("  Feature {name} (0x{page:04X}) at index 0x{idx:02X}");
                    self.features.insert(page, idx);
                }
                Ok(None) => {
                    debug!("  Feature {name} (0x{page:04X}) not supported");
                }
                Err(e) => {
                    warn!("  Feature {name} (0x{page:04X}) query failed: {e}");
                }
            }
        }

        Ok(())
    }

    /* Read DPI sensor information using feature 0x2201. */
    async fn read_dpi_info(
        &self,
        io: &mut DeviceIo,
        profile: &mut ProfileInfo,
    ) -> Result<()> {
        let Some(idx) = self.features.adjustable_dpi else {
            return Ok(());
        };

        let sensor_info = self
            .feature_request(io, idx, DPI_FN_GET_SENSOR_COUNT, &[0])
            .await?;
        if sensor_info[0] == 0 {
            return Ok(());
        }

        let dpi_data = self
            .feature_request(io, idx, DPI_FN_GET_SENSOR_DPI, &[0])
            .await?;
        let current_dpi = u16::from_be_bytes([dpi_data[1], dpi_data[2]]);
        let default_dpi = u16::from_be_bytes([dpi_data[3], dpi_data[4]]);

        if let Some(res) = profile.resolutions.first_mut() {
            res.dpi = Dpi::Unified(u32::from(current_dpi));
        }

        debug!("HID++ 2.0: sensor 0 DPI = {current_dpi} (default = {default_dpi})");
        Ok(())
    }

    /* Read report rate using feature 0x8060. */
    async fn read_report_rate(
        &self,
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
        }
        Ok(())
    }

    /* Read LED zone effect from the device using feature 0x8070. */
    async fn read_led_info(
        &self,
        io: &mut DeviceIo,
        profile: &mut ProfileInfo,
    ) -> Result<()> {
        let Some(idx) = self.features.color_led_effects else {
            return Ok(());
        };

        for led in &mut profile.leds {
            let zone_index = led.index as u8;
            let response = self
                .feature_request(io, idx, LED_FN_GET_ZONE_EFFECT, &[zone_index])
                .await?;

            /* response[0] = zone_index echo */
            /* response[1..12] = hidpp20_internal_led (11 bytes) */
            if response[0] != zone_index {
                warn!("LED read: zone mismatch (expected {zone_index}, got {})", response[0]);
                continue;
            }

            let payload = &response[1..1 + LED_PAYLOAD_SIZE];
            let mode_byte = payload[0];

            match mode_byte {
                LED_HW_MODE_OFF => {
                    led.mode = LedMode::Off;
                }
                LED_HW_MODE_FIXED => {
                    led.mode = LedMode::Solid;
                    led.color = Color::from_rgb(RgbColor {
                        r: payload[1],
                        g: payload[2],
                        b: payload[3],
                    });
                }
                LED_HW_MODE_CYCLE => {
                    led.mode = LedMode::Cycle;
                    led.effect_duration =
                        u32::from(u16::from_be_bytes([payload[6], payload[7]]));
                    led.brightness = u32::from(payload[8]) * 255 / 100;
                }
                LED_HW_MODE_COLOR_WAVE => {
                    led.mode = LedMode::ColorWave;
                    led.effect_duration =
                        u32::from(u16::from_be_bytes([payload[6], payload[7]]));
                    led.brightness = u32::from(payload[8]) * 255 / 100;
                }
                LED_HW_MODE_STARLIGHT => {
                    led.mode = LedMode::Starlight;
                    led.color = Color::from_rgb(RgbColor {
                        r: payload[1],
                        g: payload[2],
                        b: payload[3],
                    });
                    led.secondary_color = Color::from_rgb(RgbColor {
                        r: payload[4],
                        g: payload[5],
                        b: payload[6],
                    });
                }
                LED_HW_MODE_BREATHING => {
                    led.mode = LedMode::Breathing;
                    led.color = Color::from_rgb(RgbColor {
                        r: payload[1],
                        g: payload[2],
                        b: payload[3],
                    });
                    led.effect_duration =
                        u32::from(u16::from_be_bytes([payload[4], payload[5]]));
                    led.brightness = u32::from(payload[7]) * 255 / 100;
                }
                _ => {
                    debug!("LED zone {zone_index}: unknown mode 0x{mode_byte:02X}");
                }
            }

            debug!("LED zone {zone_index}: mode={:?}", led.mode);
        }

        Ok(())
    }

    /* Write LED zone effect to the device using feature 0x8070. */
    /* TriColor mode is routed through feature 0x8071 (RGB Effects) instead. */
    async fn write_led_info(
        &self,
        io: &mut DeviceIo,
        profile: &ProfileInfo,
    ) -> Result<()> {
        for led in &profile.leds {
            let zone_index = led.index as u8;

            if led.mode == LedMode::TriColor {
                /* TriColor uses 0x8071 RGB Effects with the multi-LED cluster pattern command. */
                let Some(idx) = self.features.rgb_effects else {
                    warn!("TriColor requested but device lacks RGB Effects (0x8071)");
                    continue;
                };
                let led_payload = hidpp::build_led_payload(led);

                /* Multi-LED pattern: [zone_index, ...payload..., 0x01 (persist)] */
                let mut params = [0u8; 13];
                params[0] = zone_index;
                params[1..12].copy_from_slice(&led_payload);
                params[12] = 0x01;

                /* Function 0x02 = setMultiLEDRGBClusterPattern on 0x8071 */
                self.feature_request(io, idx, 0x02, &params)
                    .await
                    .context("Failed to write TriColor multi-LED cluster pattern")?;
            } else {
                let Some(idx) = self.features.color_led_effects else {
                    warn!("Device lacks Color LED Effects (0x8070)");
                    continue;
                };
                let led_payload = hidpp::build_led_payload(led);

                /* Param layout: [zone_index, ...11-byte payload..., 0x01 (persist to flash)] */
                let mut params = [0u8; 13];
                params[0] = zone_index;
                params[1..12].copy_from_slice(&led_payload);
                params[12] = 0x01;

                self.feature_request(io, idx, LED_FN_SET_ZONE_EFFECT, &params)
                    .await
                    .context("Failed to write LED zone effect")?;
            }

            debug!("HID++ 2.0: committed LED zone {zone_index} mode={:?}", led.mode);
        }

        Ok(())
    }

    /* Write DPI sensor information using feature 0x2201. */
    async fn write_dpi_info(
        &self,
        io: &mut DeviceIo,
        profile: &ProfileInfo,
    ) -> Result<()> {
        const DPI_FN_SET_SENSOR_DPI: u8 = 0x02;

        let Some(idx) = self.features.adjustable_dpi else {
            return Ok(());
        };

        if let Some(res) = profile.resolutions.iter().find(|r| r.is_active)
            && let Dpi::Unified(dpi_val) = res.dpi
        {
            let bytes = (dpi_val as u16).to_be_bytes();
            /* Param layout: sensor (1 byte), DPI uint16 (2 bytes) */
            self.feature_request(io, idx, DPI_FN_SET_SENSOR_DPI, &[0, bytes[0], bytes[1]])
                .await
                .context("Failed to write DPI")?;
            debug!("HID++ 2.0: committed DPI = {}", dpi_val);
        }
        Ok(())
    }

    /* Write report rate using feature 0x8060. */
    async fn write_report_rate(
        &self,
        io: &mut DeviceIo,
        profile: &ProfileInfo,
    ) -> Result<()> {
        const RATE_FN_SET_REPORT_RATE: u8 = 0x02;

        let Some(idx) = self.features.report_rate else {
            return Ok(());
        };

        if profile.report_rate > 0 {
            let rate_ms = (1000 / profile.report_rate) as u8;
            self.feature_request(io, idx, RATE_FN_SET_REPORT_RATE, &[rate_ms])
                .await
                .context("Failed to write report rate")?;
            debug!("HID++ 2.0: committed report rate = {} Hz", profile.report_rate);
        }
        Ok(())
    }
}

#[async_trait]
impl super::DeviceDriver for Hidpp20Driver {
    fn name(&self) -> &str {
        "Logitech HID++ 2.0"
    }

    async fn probe(&mut self, io: &mut DeviceIo) -> Result<()> {
        let request = hidpp::build_hidpp20_request(
            self.device_index,
            ROOT_FEATURE_INDEX,
            ROOT_FN_GET_PROTOCOL_VERSION,
            SW_ID,
            &[],
        );

        let dev_idx = self.device_index;
        let (major, minor) = io
            .request(&request, 3, move |buf| {
                let report = HidppReport::parse(buf)?;
                if report.is_error() {
                    return None;
                }
                if !report.matches_hidpp20(dev_idx, ROOT_FEATURE_INDEX) {
                    return None;
                }
                if let HidppReport::Long { params, .. } = report {
                    Some((params[0], params[1]))
                } else {
                    None
                }
            })
            .await
            .context("HID++ 2.0 protocol version probe failed")?;

        self.version = ProtocolVersion { major, minor };
        info!("HID++ 2.0 device detected (protocol {major}.{minor})");

        self.discover_features(io).await?;
        Ok(())
    }

    async fn load_profiles(
        &mut self,
        io: &mut DeviceIo,
        info: &mut DeviceInfo,
    ) -> Result<()> {
        for profile in &mut info.profiles {
            if let Err(e) = self.read_dpi_info(io, profile).await {
                warn!("Failed to read DPI for profile {}: {e}", profile.index);
            }
            if let Err(e) = self.read_report_rate(io, profile).await {
                warn!("Failed to read report rate for profile {}: {e}", profile.index);
            }
            if let Err(e) = self.read_led_info(io, profile).await {
                warn!("Failed to read LEDs for profile {}: {e}", profile.index);
            }
        }

        debug!("HID++ 2.0: loaded {} profiles", info.profiles.len());
        Ok(())
    }

    async fn commit(&mut self, io: &mut DeviceIo, info: &DeviceInfo) -> Result<()> {
        if let Some(profile) = info.profiles.iter().find(|p| p.is_active) {
            if let Err(e) = self.write_dpi_info(io, profile).await {
                warn!("Failed to commit DPI for profile {}: {e:#}", profile.index);
            }
            if let Err(e) = self.write_report_rate(io, profile).await {
                warn!("Failed to commit report rate for profile {}: {e:#}", profile.index);
            }
            if let Err(e) = self.write_led_info(io, profile).await {
                warn!("Failed to commit LEDs for profile {}: {e:#}", profile.index);
            }
        }
        Ok(())
    }
}
