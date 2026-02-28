/* Canonical device state shared across DBus objects and drivers: device/profile/resolution/button
 * and LED structures plus enums for actions, DPI, and LED modes. */
/// Button action types exposed over DBus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum ActionType {
    #[default]
    None = 0,
    Button = 1,
    Special = 2,
    Key = 3,
    Macro = 4,
    Unknown = 1000,
}

impl ActionType {
    /// Convert a raw DBus `u32` value into an `ActionType`.
    /// Unknown discriminants map to [`ActionType::Unknown`].
    pub fn from_u32(val: u32) -> Self {
        match val {
            0 => Self::None,
            1 => Self::Button,
            2 => Self::Special,
            3 => Self::Key,
            4 => Self::Macro,
            _ => Self::Unknown,
        }
    }
}

/* Compact RGB color used for LED effect payloads. */
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/* Color as an RGB triplet exposed over DBus (u32 fields for compatibility). */
#[derive(Debug, Clone, Copy, Default)]
pub struct Color {
    pub red: u32,
    pub green: u32,
    pub blue: u32,
}

impl Color {
    /* Convert a DBus Color into a compact RgbColor, clamping to u8 range. */
    pub fn to_rgb(self) -> RgbColor {
        RgbColor {
            r: self.red.min(255) as u8,
            g: self.green.min(255) as u8,
            b: self.blue.min(255) as u8,
        }
    }

    /* Build a DBus Color from a compact RgbColor. */
    pub fn from_rgb(rgb: RgbColor) -> Self {
        Self {
            red: u32::from(rgb.r),
            green: u32::from(rgb.g),
            blue: u32::from(rgb.b),
        }
    }
}

/* LED effect modes matching the HID++ 2.0 protocol values. */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LedMode {
    Off = 0,
    Solid = 1,
    Cycle = 3,
    ColorWave = 4,
    Starlight = 5,
    Breathing = 10,
    TriColor = 32,
}

impl LedMode {
    /* Convert a raw DBus u32 value into a LedMode. */
    pub fn from_u32(val: u32) -> Option<LedMode> {
        match val {
            0 => Some(LedMode::Off),
            1 => Some(LedMode::Solid),
            3 => Some(LedMode::Cycle),
            4 => Some(LedMode::ColorWave),
            5 => Some(LedMode::Starlight),
            10 => Some(LedMode::Breathing),
            32 => Some(LedMode::TriColor),
            _ => None,
        }
    }
}

/* Resolution value, either unified or per-axis. */
#[derive(Debug, Clone, Copy, Default)]
pub enum Dpi {
    #[default]
    Unknown,
    Unified(u32),
    Separate {
        x: u32,
        y: u32,
    },
}

/* Device state synced from hardware. */
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub sysname: String,
    pub name: String,
    pub model: String,
    pub firmware_version: String,
    pub profiles: Vec<ProfileInfo>,
    pub driver_config: crate::device_database::DriverConfig,
}

impl DeviceInfo {
    /* Build a `DeviceInfo` struct from a matched `DeviceEntry` and detected hardware props. */
    pub fn from_entry(
        sysname: &str,
        name: &str,
        bustype: u16,
        vid: u16,
        pid: u16,
        entry: &crate::device_database::DeviceEntry,
    ) -> Self {
        let model = format!(
            "{}:{:04x}:{:04x}:0",
            crate::device_database::BusType::from_u16(bustype),
            vid,
            pid
        );

        /* Use the driver config to determine the number of profiles, buttons, etc. */
        let num_profiles = entry
            .driver_config
            .as_ref()
            .and_then(|c| c.profiles)
            .unwrap_or(1) as usize;
        let num_buttons = entry
            .driver_config
            .as_ref()
            .and_then(|c| c.buttons)
            .unwrap_or(0) as usize;
        let num_leds = entry
            .driver_config
            .as_ref()
            .and_then(|c| c.leds)
            .unwrap_or(0) as usize;
        let num_dpis = entry
            .driver_config
            .as_ref()
            .and_then(|c| c.dpis)
            .unwrap_or(1) as usize;

        /* Build DPI list from the range specification if available */
        let dpi_list: Vec<u32> = entry
            .driver_config
            .as_ref()
            .and_then(|c| c.dpi_range.as_ref())
            .map(|r| (r.min..=r.max).step_by(r.step as usize).collect())
            .unwrap_or_else(|| vec![800, 1600]);

        let profiles: Vec<ProfileInfo> = (0..num_profiles as u32)
            .map(|idx| ProfileInfo {
                index: idx,
                name: String::new(),
                is_active: idx == 0,
                is_enabled: true,
                is_dirty: false,
                report_rate: 1000,
                report_rates: vec![125, 250, 500, 1000],
                angle_snapping: -1,
                debounce: -1,
                debounces: Vec::new(),
                resolutions: (0..num_dpis as u32)
                    .map(|ri| ResolutionInfo {
                        index: ri,
                        dpi: Dpi::Unified(800),
                        dpi_list: dpi_list.clone(),
                        capabilities: Vec::new(),
                        is_active: ri == 0,
                        is_default: ri == 0,
                        is_disabled: false,
                    })
                    .collect(),
                buttons: (0..num_buttons as u32)
                    .map(|bi| ButtonInfo {
                        index: bi,
                        action_type: ActionType::Button,
                        action_types: vec![0, 1, 2, 3, 4],
                        mapping_value: bi,
                        macro_entries: Vec::new(),
                    })
                    .collect(),
                leds: (0..num_leds as u32)
                    .map(|li| LedInfo {
                        index: li,
                        mode: LedMode::Off,
                        modes: vec![
                            LedMode::Off,
                            LedMode::Solid,
                            LedMode::Cycle,
                            LedMode::ColorWave,
                            LedMode::Starlight,
                            LedMode::Breathing,
                            LedMode::TriColor,
                        ],
                        color: Color::default(),
                        secondary_color: Color::default(),
                        tertiary_color: Color::default(),
                        color_depth: 1,
                        effect_duration: 0,
                        brightness: 255,
                    })
                    .collect(),
            })
            .collect();

        Self {
            sysname: sysname.to_string(),
            name: name.to_string(),
            model,
            firmware_version: String::new(),
            profiles,
            driver_config: entry.driver_config.clone().unwrap_or_default(),
        }
    }
}

impl DeviceInfo {
    /// Find a profile by its `index` field.
    pub fn find_profile(&self, id: u32) -> Option<&ProfileInfo> {
        self.profiles.iter().find(|p| p.index == id)
    }

    /// Find a mutable profile by its `index` field.
    pub fn find_profile_mut(&mut self, id: u32) -> Option<&mut ProfileInfo> {
        self.profiles.iter_mut().find(|p| p.index == id)
    }
}

/// Profile state.
#[derive(Debug, Clone, Default)]
pub struct ProfileInfo {
    pub index: u32,
    pub name: String,
    pub is_active: bool,
    pub is_enabled: bool,
    pub is_dirty: bool,
    pub report_rate: u32,
    pub report_rates: Vec<u32>,
    pub angle_snapping: i32,
    pub debounce: i32,
    pub debounces: Vec<u32>,
    pub resolutions: Vec<ResolutionInfo>,
    pub buttons: Vec<ButtonInfo>,
    pub leds: Vec<LedInfo>,
}

impl ProfileInfo {
    /// Find a resolution by its `index` field.
    pub fn find_resolution(&self, id: u32) -> Option<&ResolutionInfo> {
        self.resolutions.iter().find(|r| r.index == id)
    }

    /// Find a mutable resolution by its `index` field.
    pub fn find_resolution_mut(&mut self, id: u32) -> Option<&mut ResolutionInfo> {
        self.resolutions.iter_mut().find(|r| r.index == id)
    }

    /// Find a button by its `index` field.
    pub fn find_button(&self, id: u32) -> Option<&ButtonInfo> {
        self.buttons.iter().find(|b| b.index == id)
    }

    /// Find a mutable button by its `index` field.
    pub fn find_button_mut(&mut self, id: u32) -> Option<&mut ButtonInfo> {
        self.buttons.iter_mut().find(|b| b.index == id)
    }

    /// Find an LED by its `index` field.
    pub fn find_led(&self, id: u32) -> Option<&LedInfo> {
        self.leds.iter().find(|l| l.index == id)
    }

    /// Find a mutable LED by its `index` field.
    pub fn find_led_mut(&mut self, id: u32) -> Option<&mut LedInfo> {
        self.leds.iter_mut().find(|l| l.index == id)
    }
}

/// Resolution state.
#[derive(Debug, Clone, Default)]
pub struct ResolutionInfo {
    pub index: u32,
    pub dpi: Dpi,
    pub dpi_list: Vec<u32>,
    pub capabilities: Vec<u32>,
    pub is_active: bool,
    pub is_default: bool,
    pub is_disabled: bool,
}

/// Button mapping state.
#[derive(Debug, Clone, Default)]
pub struct ButtonInfo {
    pub index: u32,
    pub action_type: ActionType,
    pub action_types: Vec<u32>,
    pub mapping_value: u32,
    pub macro_entries: Vec<(u32, u32)>,
}

/// LED state.
#[derive(Debug, Clone)]
pub struct LedInfo {
    pub index: u32,
    pub mode: LedMode,
    pub modes: Vec<LedMode>,
    pub color: Color,
    pub secondary_color: Color,
    pub tertiary_color: Color,
    pub color_depth: u32,
    pub effect_duration: u32,
    pub brightness: u32,
}
