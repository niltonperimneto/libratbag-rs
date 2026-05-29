/* Dev-hooks synthetic device definitions: JSON spec parsing and conversion into DeviceInfo for test
 * devices injected via the Manager when built with the dev-hooks feature. */
/// JSON-based synthetic test device specification.
///
/// The format mirrors the C `ratbagd-json.c` parser so that existing
/// Python test scripts work against the Rust daemon without modification.
///
/// Only compiled when the `dev-hooks` feature is enabled.
#[cfg(feature = "dev-hooks")]
pub mod spec {
    use serde::Deserialize;

    use crate::engine::device::{
        ActionType, ButtonInfo, Color, DeviceInfo, Dpi, LedInfo, LedMode, ProfileInfo,
        ResolutionInfo,
    };
    use crate::engine::device_database::DriverConfig;

    /* ------------------------------------------------------------------ */
    /* JSON DTOs                                                            */
    /* ------------------------------------------------------------------ */

    #[derive(Debug, Default, Deserialize)]
    pub struct TestDeviceSpec {
        #[serde(default)]
        pub profiles: Vec<TestProfileSpec>,
    }

    #[derive(Debug, Default, Deserialize)]
    pub struct TestProfileSpec {
        #[serde(default)]
        pub is_active: bool,
        #[serde(default = "default_true")]
        pub is_default: bool,
        #[serde(default)]
        pub is_disabled: bool,
        /// Polling rate in Hz.
        #[serde(default = "default_rate")]
        pub rate: u32,
        #[serde(default = "default_report_rates")]
        pub report_rates: Vec<u32>,
        #[serde(default)]
        pub resolutions: Vec<TestResolutionSpec>,
        #[serde(default)]
        pub buttons: Vec<TestButtonSpec>,
        #[serde(default)]
        pub leds: Vec<TestLedSpec>,
    }

    #[derive(Debug, Default, Deserialize)]
    pub struct TestResolutionSpec {
        #[serde(default = "default_dpi")]
        pub xres: u32,
        #[serde(default = "default_dpi")]
        pub yres: u32,
        pub dpi_min: Option<u32>,
        pub dpi_max: Option<u32>,
        #[serde(default)]
        pub is_active: bool,
        #[serde(default = "default_true")]
        pub is_default: bool,
        #[serde(default)]
        pub is_disabled: bool,
        #[serde(default)]
        pub capabilities: Vec<u32>,
    }

    #[derive(Debug, Default, Deserialize)]
    pub struct TestButtonSpec {
        #[serde(default = "default_action_type")]
        pub action_type: String,
        #[serde(default)]
        pub button: u32,
        #[serde(default)]
        pub key: u32,
    }

    #[derive(Debug, Default, Deserialize)]
    pub struct TestLedSpec {
        #[serde(default)]
        pub mode: u32,
        #[serde(default)]
        pub duration: u32,
        #[serde(default = "default_brightness")]
        pub brightness: u32,
        /// `[r, g, b]` array.
        pub color: Option<Vec<u8>>,
    }

    /* ------------------------------------------------------------------ */
    /* Defaults                                                             */
    /* ------------------------------------------------------------------ */

    fn default_true() -> bool {
        true
    }
    fn default_rate() -> u32 {
        1000
    }
    fn default_report_rates() -> Vec<u32> {
        vec![125, 250, 500, 1000]
    }
    fn default_dpi() -> u32 {
        1000
    }
    fn default_action_type() -> String {
        "button".to_string()
    }
    fn default_brightness() -> u32 {
        100
    }

    /* ------------------------------------------------------------------ */
    /* Minimum sane defaults (matches C ratbagd default_device_descr)      */
    /* ------------------------------------------------------------------ */

    fn default_resolution() -> TestResolutionSpec {
        TestResolutionSpec {
            xres: 1000,
            yres: 1000,
            dpi_min: Some(1000),
            dpi_max: Some(1000),
            is_active: true,
            is_default: true,
            is_disabled: false,
            capabilities: Vec::new(),
        }
    }

    fn default_button() -> TestButtonSpec {
        TestButtonSpec {
            action_type: "button".to_string(),
            button: 0,
            key: 0,
        }
    }

    fn default_profile() -> TestProfileSpec {
        TestProfileSpec {
            is_active: true,
            is_default: true,
            is_disabled: false,
            rate: 1000,
            report_rates: vec![125, 250, 500, 1000],
            resolutions: vec![default_resolution()],
            buttons: vec![default_button()],
            leds: Vec::new(),
        }
    }

    /* ------------------------------------------------------------------ */
    /* Conversion: spec → DeviceInfo                                        */
    /* ------------------------------------------------------------------ */

    /// Build a synthetic [`DeviceInfo`] from a parsed [`TestDeviceSpec`].
    ///
    /// If the spec has no profiles, one minimal default profile is used
    /// (matching the C daemon's `default_device_descr`).
    pub fn build_device_info(sysname: &str, mut spec: TestDeviceSpec) -> DeviceInfo {
        if spec.profiles.is_empty() {
            spec.profiles.push(default_profile());
        }

        let profiles: Vec<ProfileInfo> = spec
            .profiles
            .into_iter()
            .enumerate()
            .map(|(pi, mut p)| {
                /* Fill in minimum-sane defaults if absent */
                if p.resolutions.is_empty() {
                    p.resolutions.push(default_resolution());
                }
                if p.buttons.is_empty() {
                    p.buttons.push(default_button());
                }

                let resolutions: Vec<ResolutionInfo> = p
                    .resolutions
                    .into_iter()
                    .enumerate()
                    .map(|(ri, r)| {
                        let dpi_list = match (r.dpi_min, r.dpi_max) {
                            (Some(lo), Some(hi)) => {
                                let step = if hi - lo > 0 { 100u32 } else { 1 };
                                (lo..=hi).step_by(step as usize).collect()
                            }
                            _ => vec![r.xres],
                        };
                        ResolutionInfo {
                            index: ri as u32,
                            dpi: if r.xres == r.yres {
                                Dpi::Unified(r.xres)
                            } else {
                                Dpi::Separate {
                                    x: r.xres,
                                    y: r.yres,
                                }
                            },
                            dpi_list,
                            capabilities: r.capabilities,
                            is_active: r.is_active,
                            is_default: r.is_default,
                            is_disabled: r.is_disabled,
                        }
                    })
                    .collect();

                let buttons: Vec<ButtonInfo> = p
                    .buttons
                    .into_iter()
                    .enumerate()
                    .map(|(bi, b)| {
                        let action_type = match b.action_type.as_str() {
                            "none" => ActionType::None,
                            "button" => ActionType::Button,
                            "special" => ActionType::Special,
                            "key" => ActionType::Key,
                            "macro" => ActionType::Macro,
                            _ => ActionType::Unknown,
                        };
                        ButtonInfo {
                            index: bi as u32,
                            action_type,
                            action_types: vec![0, 1, 2, 3, 4],
                            mapping_value: b.button,
                            macro_entries: Vec::new(),
                        }
                    })
                    .collect();

                let leds: Vec<LedInfo> = p
                    .leds
                    .into_iter()
                    .enumerate()
                    .map(|(li, l)| {
                        let color = l
                            .color
                            .as_deref()
                            .and_then(|c| {
                                if c.len() >= 3 {
                                    Some(Color {
                                        red: u32::from(c[0]),
                                        green: u32::from(c[1]),
                                        blue: u32::from(c[2]),
                                    })
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();

                        LedInfo {
                            index: li as u32,
                            mode: LedMode::from_u32(l.mode).unwrap_or(LedMode::Off),
                            modes: vec![
                                LedMode::Off,
                                LedMode::Solid,
                                LedMode::Cycle,
                                LedMode::ColorWave,
                                LedMode::Breathing,
                            ],
                            color,
                            secondary_color: Color::default(),
                            tertiary_color: Color::default(),
                            color_depth: 1,
                            effect_duration: l.duration,
                            brightness: l.brightness,
                        }
                    })
                    .collect();

                ProfileInfo {
                    index: pi as u32,
                    name: String::new(),
                    is_active: p.is_active,
                    is_enabled: !p.is_disabled,
                    is_dirty: false,
                    report_rate: p.rate,
                    report_rates: p.report_rates,
                    angle_snapping: -1,
                    debounce: -1,
                    debounces: Vec::new(),
                    capabilities: Vec::new(),
                    resolutions,
                    buttons,
                    leds,
                }
            })
            .collect();

        DeviceInfo {
            sysname: sysname.to_string(),
            name: format!("Test Device ({})", sysname),
            model: "test:0000:0000:0".to_string(),
            firmware_version: String::new(),
            device_type: 2, /* mouse */
            profiles,
            driver_config: DriverConfig::default(),
        }
    }

    /// Parse a JSON string into a [`TestDeviceSpec`].
    ///
    /// An empty or `"{}"` JSON object produces the minimum sane defaults.
    pub fn parse_json(json: &str) -> Result<TestDeviceSpec, serde_json::Error> {
        /* Empty string → minimum device */
        if json.trim().is_empty() {
            return Ok(TestDeviceSpec::default());
        }
        serde_json::from_str(json)
    }
}
