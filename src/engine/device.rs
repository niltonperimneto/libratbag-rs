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

/* Canonical special-action values exposed over the DBus `mapping` property
 * when `ActionType::Special` is active.  These mirror the C libratbag enum
 * `ratbag_button_action_special` (starting at 1 << 30 = 0x4000_0000) so
 * that Piper and other DBus clients recognise every value.  Drivers must
 * translate between their hardware-specific bytecodes and these constants
 * when populating or consuming `ButtonInfo::mapping_value`. */
#[allow(dead_code)]
pub mod special_action {
    pub const BASE:                  u32 = 1 << 30;
    pub const UNKNOWN:               u32 = BASE;
    pub const DOUBLECLICK:           u32 = BASE + 1;
    pub const WHEEL_LEFT:            u32 = BASE + 2;
    pub const WHEEL_RIGHT:           u32 = BASE + 3;
    pub const WHEEL_UP:              u32 = BASE + 4;
    pub const WHEEL_DOWN:            u32 = BASE + 5;
    pub const RATCHET_MODE_SWITCH:   u32 = BASE + 6;
    pub const RESOLUTION_CYCLE_UP:   u32 = BASE + 7;
    pub const RESOLUTION_CYCLE_DOWN: u32 = BASE + 8;
    pub const RESOLUTION_UP:         u32 = BASE + 9;
    pub const RESOLUTION_DOWN:       u32 = BASE + 10;
    pub const RESOLUTION_ALTERNATE:  u32 = BASE + 11;
    pub const RESOLUTION_DEFAULT:    u32 = BASE + 12;
    pub const PROFILE_CYCLE_UP:      u32 = BASE + 13;
    pub const PROFILE_CYCLE_DOWN:    u32 = BASE + 14;
    pub const PROFILE_UP:            u32 = BASE + 15;
    pub const PROFILE_DOWN:          u32 = BASE + 16;
    pub const SECOND_MODE:           u32 = BASE + 17;
    pub const BATTERY_LEVEL:         u32 = BASE + 18;
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

/* LED effect modes exposed over DBus.
 * Discriminant values 0–3 match the C daemon's ratbag_led_mode enum
 * (Off=0, On=1, Cycle=2, Breathing=3) so that existing clients like
 * Piper work without translation.  Values 4+ are Rust-only extensions
 * for hardware modes not present in the C codebase. */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LedMode {
    Off = 0,
    Solid = 1,
    Cycle = 2,
    Breathing = 3,
    ColorWave = 4,
    Starlight = 5,
    TriColor = 6,
}

impl LedMode {
    /* Convert a raw DBus u32 value into a LedMode. */
    pub fn from_u32(val: u32) -> Option<LedMode> {
        match val {
            0 => Some(LedMode::Off),
            1 => Some(LedMode::Solid),
            2 => Some(LedMode::Cycle),
            3 => Some(LedMode::Breathing),
            4 => Some(LedMode::ColorWave),
            5 => Some(LedMode::Starlight),
            6 => Some(LedMode::TriColor),
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
    /* Device type exposed over DBus: 0=unspecified, 1=other, 2=mouse, 3=keyboard */
    pub device_type: u32,
    pub profiles: Vec<ProfileInfo>,
    pub driver_config: crate::engine::device_database::DriverConfig,
}

impl DeviceInfo {
    /* Build a `DeviceInfo` struct from a matched `DeviceEntry` and detected hardware props. */
    pub fn from_entry(
        sysname: &str,
        name: &str,
        bustype: u16,
        vid: u16,
        pid: u16,
        entry: &crate::engine::device_database::DeviceEntry,
    ) -> Self {
        let model = format!(
            "{}:{:04x}:{:04x}:0",
            crate::engine::device_database::BusType::from_u16(bustype),
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
                capabilities: Vec::new(),
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

        /* Map the .device file's DeviceType string to the DBus integer enum. */
        let device_type = match entry.device_type.to_lowercase().as_str() {
            "mouse" => 2,
            "keyboard" => 3,
            "other" => 1,
            _ => 0, /* unspecified */
        };

        Self {
            sysname: sysname.to_string(),
            name: name.to_string(),
            model,
            firmware_version: String::new(),
            device_type,
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

    pub fn with_resolution_disabled(&self, profile_id: u32, resolution_id: u32, disabled: bool) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            if let Some(res) = profile.find_resolution_mut(resolution_id) {
                res.is_disabled = disabled;
                profile.is_dirty = true;
            }
        }
        next
    }

    pub fn with_resolution_dpi(&self, profile_id: u32, resolution_id: u32, dpi: Dpi) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            if let Some(res) = profile.find_resolution_mut(resolution_id) {
                res.dpi = dpi;
                profile.is_dirty = true;
            }
        }
        next
    }

    pub fn with_active_resolution(&self, profile_id: u32, resolution_id: u32) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            for res in &mut profile.resolutions {
                res.is_active = false;
            }
            if let Some(res) = profile.find_resolution_mut(resolution_id) {
                res.is_active = true;
                profile.is_dirty = true;
            }
        }
        next
    }

    pub fn with_default_resolution(&self, profile_id: u32, resolution_id: u32) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            for res in &mut profile.resolutions {
                res.is_default = false;
            }
            if let Some(res) = profile.find_resolution_mut(resolution_id) {
                res.is_default = true;
                profile.is_dirty = true;
            }
        }
        next
    }

    pub fn with_profile_name(&self, profile_id: u32, name: String) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            profile.name = name;
            profile.is_dirty = true;
        }
        next
    }

    pub fn with_profile_disabled(&self, profile_id: u32, disabled: bool) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            profile.is_enabled = !disabled;
            profile.is_dirty = true;
        }
        next
    }

    pub fn with_profile_angle_snapping(&self, profile_id: u32, value: i32) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            profile.angle_snapping = value;
            profile.is_dirty = true;
        }
        next
    }

    pub fn with_profile_debounce(&self, profile_id: u32, value: i32) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            profile.debounce = value;
            profile.is_dirty = true;
        }
        next
    }

    pub fn with_profile_report_rate(&self, profile_id: u32, rate: u32) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            profile.report_rate = rate;
            profile.is_dirty = true;
        }
        next
    }

    pub fn with_active_profile(&self, profile_id: u32) -> Self {
        let mut next = self.clone();
        for profile in &mut next.profiles {
            profile.is_active = false;
        }
        if let Some(profile) = next.find_profile_mut(profile_id) {
            profile.is_active = true;
            profile.is_dirty = true;
        }
        next
    }

    pub fn with_button_mapping(
        &self,
        profile_id: u32,
        button_id: u32,
        action_type: ActionType,
        mapping_value: u32,
        macro_entries: Vec<(u32, u32)>,
    ) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            if let Some(button) = profile.find_button_mut(button_id) {
                button.action_type = action_type;
                button.mapping_value = mapping_value;
                button.macro_entries = macro_entries;
                profile.is_dirty = true;
            }
        }
        next
    }

    pub fn with_led_mode(&self, profile_id: u32, led_id: u32, mode: LedMode) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            if let Some(led) = profile.find_led_mut(led_id) {
                led.mode = mode;
                profile.is_dirty = true;
            }
        }
        next
    }

    pub fn with_led_color(&self, profile_id: u32, led_id: u32, color: Color) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            if let Some(led) = profile.find_led_mut(led_id) {
                led.color = color;
                profile.is_dirty = true;
            }
        }
        next
    }

    pub fn with_led_secondary_color(&self, profile_id: u32, led_id: u32, color: Color) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            if let Some(led) = profile.find_led_mut(led_id) {
                led.secondary_color = color;
                profile.is_dirty = true;
            }
        }
        next
    }

    pub fn with_led_tertiary_color(&self, profile_id: u32, led_id: u32, color: Color) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            if let Some(led) = profile.find_led_mut(led_id) {
                led.tertiary_color = color;
                profile.is_dirty = true;
            }
        }
        next
    }

    pub fn with_led_effect_duration(&self, profile_id: u32, led_id: u32, duration: u32) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            if let Some(led) = profile.find_led_mut(led_id) {
                led.effect_duration = duration;
                profile.is_dirty = true;
            }
        }
        next
    }

    pub fn with_led_brightness(&self, profile_id: u32, led_id: u32, brightness: u32) -> Self {
        let mut next = self.clone();
        if let Some(profile) = next.find_profile_mut(profile_id) {
            if let Some(led) = profile.find_led_mut(led_id) {
                led.brightness = brightness;
                profile.is_dirty = true;
            }
        }
        next
    }

    pub fn with_cleared_dirty_flags(&self) -> Self {
        let mut next = self.clone();
        for profile in &mut next.profiles {
            profile.is_dirty = false;
        }
        next
    }
}

/* Profile capability constants matching libratbag's `ratbag_profile_capability` enum.
 * Only SET_DEFAULT and DISABLE are exposed over DBus (matching the C daemon). */
pub const RATBAG_PROFILE_CAP_SET_DEFAULT: u32 = 101;
pub const RATBAG_PROFILE_CAP_DISABLE: u32 = 102;

/* Resolution capability constants matching libratbag's `ratbag_resolution_capability` enum.
 * SEPARATE_XY gates whether a (u32,u32) DPI tuple is accepted over DBus;
 * DISABLE gates whether the is_disabled property can be toggled. */
pub const RATBAG_RESOLUTION_CAP_INDIVIDUAL_REPORT_RATE: u32 = 1;
pub const RATBAG_RESOLUTION_CAP_SEPARATE_XY_RESOLUTION: u32 = 2;
pub const RATBAG_RESOLUTION_CAP_DISABLE: u32 = 3;

/// Minimum and maximum allowed report rates (Hz) for sanity-clamping.
pub const REPORT_RATE_MIN: u32 = 125;
pub const REPORT_RATE_MAX: u32 = 8000;

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
    pub capabilities: Vec<u32>,
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

    /// Clamp a report rate to the allowed range.
    #[inline]
    pub fn clamp_report_rate(rate: u32) -> u32 {
        rate.clamp(REPORT_RATE_MIN, REPORT_RATE_MAX)
    }

    /// Return only the well-known profile capabilities (SET_DEFAULT, DISABLE)
    /// that are present in this profile's capability list.
    pub fn dbus_capabilities(&self) -> Vec<u32> {
        const EXPOSED: &[u32] = &[RATBAG_PROFILE_CAP_SET_DEFAULT, RATBAG_PROFILE_CAP_DISABLE];
        self.capabilities
            .iter()
            .copied()
            .filter(|c| EXPOSED.contains(c))
            .collect()
    }

    /// Sanitize a profile name for DBus transport.
    ///
    /// C-compatible policy: if the bytes are valid UTF-8, use them as-is;
    /// otherwise attempt ISO-8859-1 → UTF-8 conversion; failing that,
    /// strip non-ASCII bytes.
    pub fn sanitize_name(raw: &str) -> String {
        /* Rust strings are always valid UTF-8, so the first branch always
         * holds for data originating from Rust.  The fallback paths exist
         * for drivers that may stuff raw bytes into the name field via
         * unsafe or FFI. */
        if raw.is_ascii() || std::str::from_utf8(raw.as_bytes()).is_ok() {
            return raw.to_owned();
        }
        /* Treat each byte as ISO-8859-1 code point → char (always valid). */
        let latin1: String = raw.bytes().map(|b| b as char).collect();
        if latin1.is_empty() {
            /* If even that produced nothing, keep only ASCII. */
            raw.bytes()
                .filter(|b| b.is_ascii() && *b >= 0x20)
                .map(|b| b as char)
                .collect()
        } else {
            latin1
        }
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
