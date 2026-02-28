/* Parser and lookup for .device files: loads INI entries into DeviceDb keyed by bus/vid/pid and
 * exposes typed structs for matches and driver-specific config. */
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use configparser::ini::Ini;
use tracing::{debug, warn};

/* Bus protocol identifier used in `.device` match patterns and DB keys. */
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BusType {
    Usb,
    Bluetooth,
    Other(String),
}

impl BusType {
    /* Convert the numeric bustype from a udev HID_ID attribute into a BusType. */
    pub fn from_u16(bustype: u16) -> Self {
        match bustype {
            0x03 => BusType::Usb,
            0x05 => BusType::Bluetooth,
            other => BusType::Other(format!("{:04x}", other)),
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "usb" => BusType::Usb,
            "bluetooth" => BusType::Bluetooth,
            other => BusType::Other(other.to_string()),
        }
    }
}

impl fmt::Display for BusType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BusType::Usb => f.write_str("usb"),
            BusType::Bluetooth => f.write_str("bluetooth"),
            BusType::Other(s) => f.write_str(s),
        }
    }
}

/* A parsed `.device` file entry describing a supported mouse. */
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DeviceEntry {
    pub name: String,
    pub driver: String,
    pub device_type: String,
    pub matches: Vec<DeviceMatch>,
    pub driver_config: Option<DriverConfig>,
}

/* A single bus:vid:pid match pattern from the `DeviceMatch=` field. */
#[derive(Debug, Clone)]
pub struct DeviceMatch {
    pub bustype: BusType,
    pub vid: u16,
    pub pid: u16,
}

/* Driver-specific configuration from the `[Driver/xxx]` section. */
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct DriverConfig {
    pub profiles: Option<u32>,
    pub buttons: Option<u32>,
    pub leds: Option<u32>,
    pub dpis: Option<u32>,
    pub dpi_range: Option<DpiRange>,
    pub wireless: bool,
    pub device_version: Option<u32>,
    pub macro_length: Option<u32>,
    pub quirks: Vec<String>,
    pub button_mapping: Vec<u8>,
    pub button_mapping_secondary: Vec<u8>,
    pub led_modes: Vec<String>,
}

/* A DPI range specification parsed from `DpiRange=min:max@step`. */
#[derive(Debug, Clone)]
pub struct DpiRange {
    pub min: u32,
    pub max: u32,
    pub step: u32,
}

/* Device database: maps `(bustype, vid, pid)` to a `DeviceEntry`. */
/*                                                                   */
/* Entries are reference-counted so that devices with multiple match */
/* patterns share a single allocation instead of being duplicated.   */
pub type DeviceDb = HashMap<(BusType, u16, u16), Arc<DeviceEntry>>;

/* Load all `.device` files from the given directory into a lookup table. */
/*  */
/* Each `DeviceMatch` pattern (semicolon-separated in the file) becomes */
/* a separate key in the returned map, all pointing to the same `DeviceEntry`. */
pub fn load_device_database(data_dir: &Path) -> DeviceDb {
    let mut db = HashMap::new();

    let entries = match std::fs::read_dir(data_dir) {
        Ok(e) => e,
        Err(err) => {
            warn!("Failed to read device data directory {:?}: {}", data_dir, err);
            return db;
        }
    };

    for dir_entry in entries.flatten() {
        let path = dir_entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("device") {
            continue;
        }

        match parse_device_file(&path) {
            Ok(entry) => {
                /* Collect keys first so we move BusType out of the Vec
                 * before entry is frozen inside the Arc. */
                let keys: Vec<(BusType, u16, u16)> = entry
                    .matches
                    .iter()
                    .map(|m| (m.bustype.clone(), m.vid, m.pid))
                    .collect();
                let entry = Arc::new(entry);
                for key in keys {
                    db.insert(key, Arc::clone(&entry));
                }
                debug!(
                    "Loaded device: {} ({} match patterns)",
                    entry.name,
                    entry.matches.len()
                );
            }
            Err(err) => {
                warn!("Failed to parse {:?}: {}", path, err);
            }
        }
    }

    debug!("Device database loaded: {} entries", db.len());
    db
}

/* Parse a single `.device` INI file into a `DeviceEntry`. */
fn parse_device_file(path: &Path) -> Result<DeviceEntry, String> {
    let mut ini = Ini::new();
    ini.load(path).map_err(|e| format!("INI parse error: {}", e))?;

    /* [Device] section — required fields */
    let name = ini
        .get("device", "name")
        .ok_or("Missing [Device] Name")?;
    let driver = ini
        .get("device", "driver")
        .ok_or("Missing [Device] Driver")?;
    let match_str = ini
        .get("device", "devicematch")
        .ok_or("Missing [Device] DeviceMatch")?;
    let device_type = ini
        .get("device", "devicetype")
        .unwrap_or_else(|| "mouse".to_string());

    /* Parse semicolon-separated match patterns: "usb:046d:c539;usb:046d:c53a" */
    let matches = parse_device_matches(&match_str)?;

    /* [Driver/xxx] section — optional */
    let driver_section = format!("driver/{}", driver);
    let has_driver_section = ini.get(&driver_section, "profiles").is_some()
        || ini.get(&driver_section, "buttons").is_some()
        || ini.get(&driver_section, "leds").is_some()
        || ini.get(&driver_section, "dpis").is_some()
        || ini.get(&driver_section, "dpirange").is_some()
        || ini.get(&driver_section, "deviceversion").is_some()
        || ini.get(&driver_section, "macrolength").is_some()
        || ini.get(&driver_section, "quirk").is_some()
        || ini.get(&driver_section, "quirks").is_some()
        || ini.get(&driver_section, "buttonmapping").is_some()
        || ini.get(&driver_section, "buttonmappingsecondary").is_some()
        || ini.get(&driver_section, "ledmodes").is_some();

    let driver_config = if has_driver_section {
        Some(parse_driver_config(&ini, &driver_section))
    } else {
        None
    };

    Ok(DeviceEntry {
        name,
        driver,
        device_type,
        matches,
        driver_config,
    })
}

/* Parse a `DeviceMatch` string like `"usb:046d:c539;usb:046d:c53a"`. */
fn parse_device_matches(s: &str) -> Result<Vec<DeviceMatch>, String> {
    let mut matches = Vec::new();

    for part in s.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        let segments: Vec<&str> = part.split(':').collect();
        if segments.len() != 3 {
            return Err(format!("Invalid DeviceMatch pattern: {}", part));
        }

        let bustype = BusType::from_str(segments[0]);
        let vid = u16::from_str_radix(segments[1], 16)
            .map_err(|e| format!("Invalid VID in '{}': {}", part, e))?;
        let pid = u16::from_str_radix(segments[2], 16)
            .map_err(|e| format!("Invalid PID in '{}': {}", part, e))?;

        matches.push(DeviceMatch { bustype, vid, pid });
    }

    if matches.is_empty() {
        return Err("DeviceMatch is empty".to_string());
    }

    Ok(matches)
}

/* Parse the `[Driver/xxx]` section for driver-specific configuration. */
fn parse_driver_config(ini: &Ini, section: &str) -> DriverConfig {
    let dpi_range = ini
        .get(section, "dpirange")
        .and_then(|s| parse_dpi_range(&s));

    /* Quirks: handle both Logitech's singular `Quirk=` and Asus's plural `Quirks=`. */
    let quirks = ini
        .get(section, "quirks")
        .or_else(|| ini.get(section, "quirk"))
        .map(|s| parse_semicolon_strings(&s))
        .unwrap_or_default();

    let button_mapping = ini
        .get(section, "buttonmapping")
        .map(|s| parse_hex_array(&s))
        .unwrap_or_default();

    let button_mapping_secondary = ini
        .get(section, "buttonmappingsecondary")
        .map(|s| parse_hex_array(&s))
        .unwrap_or_default();

    let led_modes = ini
        .get(section, "ledmodes")
        .map(|s| parse_semicolon_strings(&s))
        .unwrap_or_default();

    DriverConfig {
        profiles: ini.get(section, "profiles").and_then(|v| v.parse().ok()),
        buttons: ini.get(section, "buttons").and_then(|v| v.parse().ok()),
        leds: ini.get(section, "leds").and_then(|v| v.parse().ok()),
        dpis: ini.get(section, "dpis").and_then(|v| v.parse().ok()),
        wireless: ini
            .get(section, "wireless")
            .and_then(|v| v.parse::<u32>().ok())
            .map(|v| v != 0)
            .unwrap_or(false),
        device_version: ini
            .get(section, "deviceversion")
            .and_then(|v| v.parse().ok()),
        macro_length: ini
            .get(section, "macrolength")
            .and_then(|v| v.parse().ok()),
        dpi_range,
        quirks,
        button_mapping,
        button_mapping_secondary,
        led_modes,
    }
}

/* Split a semicolon-delimited string into trimmed, non-empty strings. */
fn parse_semicolon_strings(s: &str) -> Vec<String> {
    s.split(';')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/* Parse a semicolon-delimited list of hex values (e.g. "f0;f1;e6") into bytes. */
fn parse_hex_array(s: &str) -> Vec<u8> {
    s.split(';')
        .filter_map(|p| {
            let trimmed = p.trim();
            if trimmed.is_empty() {
                return None;
            }
            u8::from_str_radix(trimmed, 16).ok()
        })
        .collect()
}

/* Parse a DPI range string like `"100:16000@100"`. */
fn parse_dpi_range(s: &str) -> Option<DpiRange> {
    let (range_part, step_str) = s.split_once('@')?;
    let (min_str, max_str) = range_part.split_once(':')?;

    let min = min_str.parse().ok()?;
    let max = max_str.parse().ok()?;
    let step: u32 = step_str.parse().ok()?;

    /* Reject degenerate ranges that would cause step_by(0) panics or empty lists. */
    if step == 0 || min > max {
        return None;
    }

    Some(DpiRange { min, max, step })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_device_matches_single() {
        let matches = parse_device_matches("usb:046d:c539").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].bustype, BusType::Usb);
        assert_eq!(matches[0].vid, 0x046d);
        assert_eq!(matches[0].pid, 0xc539);
    }

    #[test]
    fn test_parse_device_matches_multiple() {
        let matches = parse_device_matches("usb:0b05:18e3;usb:0b05:18e5").unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].pid, 0x18e3);
        assert_eq!(matches[1].pid, 0x18e5);
    }

    #[test]
    fn test_parse_device_matches_bluetooth() {
        let matches = parse_device_matches("bluetooth:046d:b025").unwrap();
        assert_eq!(matches[0].bustype, BusType::Bluetooth);
    }

    #[test]
    fn test_parse_device_matches_mixed_bus() {
        let matches =
            parse_device_matches("usb:046d:4090;bluetooth:046d:b025").unwrap();
        assert_eq!(matches[0].bustype, BusType::Usb);
        assert_eq!(matches[1].bustype, BusType::Bluetooth);
    }

    #[test]
    fn test_parse_dpi_range() {
        let range = parse_dpi_range("100:16000@100").unwrap();
        assert_eq!(range.min, 100);
        assert_eq!(range.max, 16000);
        assert_eq!(range.step, 100);
    }

    #[test]
    fn test_parse_dpi_range_invalid() {
        assert!(parse_dpi_range("invalid").is_none());
    }

    #[test]
    fn test_parse_dpi_range_zero_step() {
        assert!(parse_dpi_range("100:16000@0").is_none());
    }

    #[test]
    fn test_parse_dpi_range_inverted_bounds() {
        assert!(parse_dpi_range("16000:100@100").is_none());
    }

    #[test]
    fn test_parse_device_matches_invalid() {
        assert!(parse_device_matches("usb:046d").is_err());
    }

    #[test]
    fn test_parse_device_matches_empty() {
        assert!(parse_device_matches("").is_err());
    }

    #[test]
    fn test_parse_semicolon_strings() {
        let result = parse_semicolon_strings("DOUBLE_DPI;RAW_BRIGHTNESS;SEPARATE_XY_DPI");
        assert_eq!(result, vec!["DOUBLE_DPI", "RAW_BRIGHTNESS", "SEPARATE_XY_DPI"]);
    }

    #[test]
    fn test_parse_semicolon_strings_single() {
        let result = parse_semicolon_strings("INDEX_OFFSET");
        assert_eq!(result, vec!["INDEX_OFFSET"]);
    }

    #[test]
    fn test_parse_semicolon_strings_empty() {
        let result = parse_semicolon_strings("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_hex_array() {
        let result = parse_hex_array("f0;f1;f2;0;0;e6;e8;e9;d0;d1;d2;d3");
        assert_eq!(result, vec![0xf0, 0xf1, 0xf2, 0x00, 0x00, 0xe6, 0xe8, 0xe9, 0xd0, 0xd1, 0xd2, 0xd3]);
    }

    #[test]
    fn test_parse_hex_array_empty() {
        let result = parse_hex_array("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_hex_array_trailing_semicolon() {
        let result = parse_hex_array("0a;0b;");
        assert_eq!(result, vec![0x0a, 0x0b]);
    }

    #[test]
    fn test_bustype_from_u16() {
        assert_eq!(BusType::from_u16(0x03), BusType::Usb);
        assert_eq!(BusType::from_u16(0x05), BusType::Bluetooth);
        assert_eq!(BusType::from_u16(0x01), BusType::Other("0001".to_string()));
    }

    #[test]
    fn test_bustype_display() {
        assert_eq!(BusType::Usb.to_string(), "usb");
        assert_eq!(BusType::Bluetooth.to_string(), "bluetooth");
        assert_eq!(BusType::Other("serial".to_string()).to_string(), "serial");
    }
}
