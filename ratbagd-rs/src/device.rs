/* Button action types exposed over DBus. */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ActionType {
    None = 0,
    Button = 1,
    Special = 2,
    Key = 3,
    Macro = 4,
    Unknown = 1000,
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
#[derive(Debug, Clone, Copy)]
pub enum Dpi {
    Unified(u32),
    Separate { x: u32, y: u32 },
}

/* Device state synced from hardware. */
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub sysname: String,
    pub name: String,
    pub model: String,
    pub firmware_version: String,
    pub profiles: Vec<ProfileInfo>,
}

impl DeviceInfo {
    /* Translate a numeric bustype from HID_ID into the string used in `.device` files. */
    fn bustype_to_string(bustype: u16) -> String {
        match bustype {
            0x03 => "usb".to_string(),
            0x05 => "bluetooth".to_string(),
            _ => format!("{:04x}", bustype),
        }
    }

    /* Build a `DeviceInfo` struct from a matched `DeviceEntry` and detected hardware props. */
    pub fn from_entry(
        sysname: &str,
        name: &str,
        bustype: u16,
        vid: u16,
        pid: u16,
        entry: &crate::device_database::DeviceEntry,
    ) -> Self {
        let bus_str = Self::bustype_to_string(bustype);
        let model = format!("{}:{:04x}:{:04x}:0", bus_str, vid, pid);

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
        }
    }
}

/* Profile state. */
#[derive(Debug, Clone)]
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

/* Resolution state. */
#[derive(Debug, Clone)]
pub struct ResolutionInfo {
    pub index: u32,
    pub dpi: Dpi,
    pub dpi_list: Vec<u32>,
    pub capabilities: Vec<u32>,
    pub is_active: bool,
    pub is_default: bool,
    pub is_disabled: bool,
}

/* Button mapping state. */
#[derive(Debug, Clone)]
pub struct ButtonInfo {
    pub index: u32,
    pub action_type: ActionType,
    pub action_types: Vec<u32>,
    pub mapping_value: u32,
    pub macro_entries: Vec<(u32, u32)>,
}

/* LED state. */
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
