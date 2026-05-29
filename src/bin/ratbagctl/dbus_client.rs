/* ratbagctl DBus client: low-level helper for calling the org.freedesktop.ratbag1 API, wrapping
 * property access and method calls for devices, profiles, resolutions, buttons, and LEDs. */
//! Low-level DBus proxy client for `org.freedesktop.ratbag1`.
//!
//! All communication with the daemon goes through this module.

use anyhow::{anyhow, Context, Result};
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

const BUS_NAME: &str = "org.freedesktop.ratbag1";
const MANAGER_PATH: &str = "/org/freedesktop/ratbag1";
const MANAGER_IFACE: &str = "org.freedesktop.ratbag1.Manager";
const DEVICE_IFACE: &str = "org.freedesktop.ratbag1.Device";
const PROFILE_IFACE: &str = "org.freedesktop.ratbag1.Profile";
const RESOLUTION_IFACE: &str = "org.freedesktop.ratbag1.Resolution";
const BUTTON_IFACE: &str = "org.freedesktop.ratbag1.Button";
const LED_IFACE: &str = "org.freedesktop.ratbag1.Led";

/// A client that talks to the `ratbagd` daemon over the system DBus.
pub struct RatbagClient {
    conn: Connection,
}

impl RatbagClient {
    /// Connect to the session bus.
    pub async fn connect() -> Result<Self> {
        let conn = Connection::session()
            .await
            .context("Cannot connect to the session DBus")?;
        Ok(Self { conn })
    }

    // -----------------------------------------------------------------------
    // Manager
    // -----------------------------------------------------------------------

    /// Get the DBus API version from the Manager.
    pub async fn get_api_version(&self) -> Result<i32> {
        self.get_i32_property(MANAGER_PATH, MANAGER_IFACE, "APIVersion").await
    }

    /// Get the list of device object paths from the Manager.
    pub async fn list_devices(&self) -> Result<Vec<String>> {
        let val = self.get_property(MANAGER_PATH, MANAGER_IFACE, "Devices").await?;
        extract_object_path_array(val).context("Failed to parse Devices property")
    }

    /// Load a synthetic test device (dev-hooks only).
    pub async fn load_test_device(&self, json: &str) -> Result<String> {
        let reply = self
            .conn
            .call_method(Some(BUS_NAME), MANAGER_PATH, Some(MANAGER_IFACE), "LoadTestDevice", &(json,))
            .await
            .context("LoadTestDevice call failed")?;
        let path: String = reply.body().deserialize()?;
        Ok(path)
    }

    /// Reset / remove all test devices (dev-hooks only).
    pub async fn reset_test_device(&self) -> Result<()> {
        self.conn
            .call_method(Some(BUS_NAME), MANAGER_PATH, Some(MANAGER_IFACE), "ResetTestDevice", &())
            .await
            .context("ResetTestDevice call failed")?;
        Ok(())
    }

    /// Resolve a device specifier (numeric index or sysname substring) to a
    /// full object path.
    pub async fn resolve_device(&self, spec: &str) -> Result<String> {
        let devices = self.list_devices().await?;
        anyhow::ensure!(!devices.is_empty(), "No devices found");

        // Try numeric index first.
        if let Ok(idx) = spec.parse::<usize>() {
            return devices
                .get(idx)
                .cloned()
                .with_context(|| format!("Device index {} out of range (0..{})", idx, devices.len()));
        }

        // Otherwise match against the path suffix (sysname).
        for path in &devices {
            if path.ends_with(spec) || path.contains(spec) {
                return Ok(path.clone());
            }
        }

        anyhow::bail!("No device matching '{}' found", spec)
    }

    // -----------------------------------------------------------------------
    // Device
    // -----------------------------------------------------------------------

    pub async fn get_device_name(&self, path: &str) -> Result<String> {
        self.get_string_property(path, DEVICE_IFACE, "Name").await
    }

    pub async fn get_device_model(&self, path: &str) -> Result<String> {
        self.get_string_property(path, DEVICE_IFACE, "Model").await
    }

    pub async fn get_device_firmware(&self, path: &str) -> Result<String> {
        self.get_string_property(path, DEVICE_IFACE, "FirmwareVersion").await
    }

    pub async fn get_device_profiles(&self, path: &str) -> Result<Vec<String>> {
        let val = self.get_property(path, DEVICE_IFACE, "Profiles").await?;
        extract_object_path_array(val).context("Failed to parse Profiles property")
    }

    pub async fn commit_device(&self, path: &str) -> Result<u32> {
        let reply = self
            .conn
            .call_method(Some(BUS_NAME), path, Some(DEVICE_IFACE), "Commit", &())
            .await
            .context("Commit call failed")?;
        let result: u32 = reply.body().deserialize()?;
        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Profile
    // -----------------------------------------------------------------------

    pub async fn get_profile_index(&self, path: &str) -> Result<u32> {
        self.get_u32_property(path, PROFILE_IFACE, "Index").await
    }

    pub async fn get_profile_name(&self, path: &str) -> Result<String> {
        self.get_string_property(path, PROFILE_IFACE, "Name").await
    }

    pub async fn set_profile_name(&self, path: &str, name: &str) -> Result<()> {
        self.set_property(path, PROFILE_IFACE, "Name", Value::from(name))
            .await
    }

    pub async fn get_profile_is_active(&self, path: &str) -> Result<bool> {
        self.get_bool_property(path, PROFILE_IFACE, "IsActive").await
    }

    pub async fn get_profile_is_dirty(&self, path: &str) -> Result<bool> {
        self.get_bool_property(path, PROFILE_IFACE, "IsDirty").await
    }

    pub async fn get_profile_disabled(&self, path: &str) -> Result<bool> {
        self.get_bool_property(path, PROFILE_IFACE, "Disabled").await
    }

    pub async fn set_profile_disabled(&self, path: &str, disabled: bool) -> Result<()> {
        self.set_property(path, PROFILE_IFACE, "Disabled", Value::from(disabled))
            .await
    }

    pub async fn get_profile_report_rate(&self, path: &str) -> Result<u32> {
        self.get_u32_property(path, PROFILE_IFACE, "ReportRate").await
    }

    pub async fn get_profile_report_rates(&self, path: &str) -> Result<Vec<u32>> {
        self.get_vec_u32_property(path, PROFILE_IFACE, "ReportRates").await
    }

    pub async fn get_profile_angle_snapping(&self, path: &str) -> Result<i32> {
        self.get_i32_property(path, PROFILE_IFACE, "AngleSnapping").await
    }

    pub async fn get_profile_debounce(&self, path: &str) -> Result<i32> {
        self.get_i32_property(path, PROFILE_IFACE, "Debounce").await
    }

    pub async fn set_profile_angle_snapping(&self, path: &str, value: i32) -> Result<()> {
        self.set_property(path, PROFILE_IFACE, "AngleSnapping", Value::from(value))
            .await
    }

    pub async fn set_profile_debounce(&self, path: &str, value: i32) -> Result<()> {
        self.set_property(path, PROFILE_IFACE, "Debounce", Value::from(value))
            .await
    }

    pub async fn get_profile_debounces(&self, path: &str) -> Result<Vec<u32>> {
        self.get_vec_u32_property(path, PROFILE_IFACE, "Debounces").await
    }

    pub async fn set_profile_report_rate(&self, path: &str, rate: u32) -> Result<()> {
        self.set_property(path, PROFILE_IFACE, "ReportRate", Value::from(rate))
            .await
    }

    pub async fn call_profile_set_active(&self, path: &str) -> Result<()> {
        self.conn
            .call_method(Some(BUS_NAME), path, Some(PROFILE_IFACE), "SetActive", &())
            .await
            .context("SetActive call failed")?;
        Ok(())
    }

    pub async fn get_profile_resolutions(&self, path: &str) -> Result<Vec<String>> {
        let val = self.get_property(path, PROFILE_IFACE, "Resolutions").await?;
        extract_object_path_array(val).context("Failed to parse Resolutions property")
    }

    pub async fn get_profile_buttons(&self, path: &str) -> Result<Vec<String>> {
        let val = self.get_property(path, PROFILE_IFACE, "Buttons").await?;
        extract_object_path_array(val).context("Failed to parse Buttons property")
    }

    pub async fn get_profile_leds(&self, path: &str) -> Result<Vec<String>> {
        let val = self.get_property(path, PROFILE_IFACE, "Leds").await?;
        extract_object_path_array(val).context("Failed to parse Leds property")
    }

    // -----------------------------------------------------------------------
    // Resolution
    // -----------------------------------------------------------------------

    pub async fn get_resolution_index(&self, path: &str) -> Result<u32> {
        self.get_u32_property(path, RESOLUTION_IFACE, "Index").await
    }

    pub async fn get_resolution_is_active(&self, path: &str) -> Result<bool> {
        self.get_bool_property(path, RESOLUTION_IFACE, "IsActive").await
    }

    pub async fn get_resolution_is_default(&self, path: &str) -> Result<bool> {
        self.get_bool_property(path, RESOLUTION_IFACE, "IsDefault").await
    }

    pub async fn get_resolution_is_disabled(&self, path: &str) -> Result<bool> {
        self.get_bool_property(path, RESOLUTION_IFACE, "IsDisabled").await
    }

    pub async fn set_resolution_is_disabled(&self, path: &str, disabled: bool) -> Result<()> {
        self.set_property(path, RESOLUTION_IFACE, "IsDisabled", Value::from(disabled))
            .await
    }

    pub async fn get_resolution_capabilities(&self, path: &str) -> Result<Vec<u32>> {
        self.get_vec_u32_property(path, RESOLUTION_IFACE, "Capabilities").await
    }

    /// Get the list of supported DPI values.
    pub async fn get_resolution_dpi_list(&self, path: &str) -> Result<Vec<u32>> {
        self.get_vec_u32_property(path, RESOLUTION_IFACE, "Resolutions").await
    }

    /// Get the DPI as a display string.
    ///
    /// The DBus property is a variant: either `u32` or `(u32, u32)`.
    pub async fn get_resolution_dpi(&self, path: &str) -> Result<String> {
        let val = self.get_property(path, RESOLUTION_IFACE, "Resolution").await?;
        let inner: Value<'_> = val.into();
        match &inner {
            Value::U32(v) => Ok(format!("{} DPI", v)),
            Value::Structure(s) => {
                if let [Value::U32(x), Value::U32(y)] = s.fields() {
                    if x == y {
                        Ok(format!("{} DPI", x))
                    } else {
                        Ok(format!("{}x{} DPI", x, y))
                    }
                } else {
                    Err(anyhow!("Malformed Resolution property at {}", path))
                }
            }
            _ => Err(anyhow!("Unexpected Resolution property type at {}", path)),
        }
    }

    pub async fn set_resolution_dpi(&self, path: &str, dpi: u32) -> Result<()> {
        let owned = OwnedValue::try_from(Value::from((dpi, dpi)))
            .map_err(|e| anyhow!("Failed to encode D-Bus value: {e}"))?;
        let wrapped = Value::Value(Box::new(owned.into()));
        self.set_property(path, RESOLUTION_IFACE, "Resolution", wrapped)
            .await
    }

    pub async fn call_resolution_set_active(&self, path: &str) -> Result<()> {
        self.conn
            .call_method(
                Some(BUS_NAME),
                path,
                Some(RESOLUTION_IFACE),
                "SetActive",
                &(),
            )
            .await
            .context("SetActive call failed")?;
        Ok(())
    }

    pub async fn call_resolution_set_default(&self, path: &str) -> Result<()> {
        self.conn
            .call_method(
                Some(BUS_NAME),
                path,
                Some(RESOLUTION_IFACE),
                "SetDefault",
                &(),
            )
            .await
            .context("SetDefault call failed")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Button
    // -----------------------------------------------------------------------

    pub async fn get_button_index(&self, path: &str) -> Result<u32> {
        self.get_u32_property(path, BUTTON_IFACE, "Index").await
    }

    /// Returns `(action_type, mapping_display_string)`.
    ///
    /// For macro mappings (type 4) the display string shows decoded key events.
    pub async fn get_button_mapping(&self, path: &str) -> Result<(u32, String)> {
        let val = self.get_property(path, BUTTON_IFACE, "Mapping").await?;
        let inner: Value<'_> = val.into();
        if let Value::Structure(s) = &inner {
            if let [Value::U32(action_type), variant] = s.fields() {
                let display = match variant {
                    Value::U32(v) => v.to_string(),
                    Value::Array(arr) => {
                        // Decode macro entries: Vec<(u32, u32)> = (keycode, direction)
                        let mut entries: Vec<String> = Vec::with_capacity(arr.len());
                        for item in arr.iter() {
                            if let Value::Structure(t) = item {
                                if let [Value::U32(keycode), Value::U32(dir)] = t.fields() {
                                    let arrow = if *dir == 1 { "↓" } else { "↑" };
                                    entries.push(format!("{}:{}", keycode, arrow));
                                    continue;
                                }
                            }
                            return Err(anyhow!("Malformed macro mapping entry at {}", path));
                        }
                        entries.join(" ")
                    }
                    _ => return Err(anyhow!("Unsupported Mapping payload type at {}", path)),
                };
                return Ok((*action_type, display));
            }
        }
        Err(anyhow!("Malformed Mapping property at {}", path))
    }

    pub async fn get_button_action_types(&self, path: &str) -> Result<Vec<u32>> {
        self.get_vec_u32_property(path, BUTTON_IFACE, "ActionTypes").await
    }

    pub async fn set_button_mapping(
        &self,
        path: &str,
        action_type: u32,
        value: u32,
    ) -> Result<()> {
        let mapping = (action_type, Value::from(value));
        self.set_property(path, BUTTON_IFACE, "Mapping", Value::from(mapping))
            .await
    }

    /// Set a macro mapping (action type 4) with a list of (keycode, direction) pairs.
    pub async fn set_button_macro_mapping(
        &self,
        path: &str,
        events: &[(u32, u32)],
    ) -> Result<()> {
        for &(keycode, direction) in events {
            anyhow::ensure!(keycode <= u16::MAX as u32, "Invalid keycode {} (max 65535)", keycode);
            anyhow::ensure!(direction <= 1, "Invalid macro direction {} (expected 0 or 1)", direction);
        }
        let arr: Vec<(u32, u32)> = events.to_vec();
        let mapping = (4u32, Value::from(arr));
        self.set_property(path, BUTTON_IFACE, "Mapping", Value::from(mapping))
            .await
    }

    // -----------------------------------------------------------------------
    // LED
    // -----------------------------------------------------------------------

    pub async fn get_led_index(&self, path: &str) -> Result<u32> {
        self.get_u32_property(path, LED_IFACE, "Index").await
    }

    pub async fn get_led_mode(&self, path: &str) -> Result<u32> {
        self.get_u32_property(path, LED_IFACE, "Mode").await
    }

    pub async fn get_led_modes(&self, path: &str) -> Result<Vec<u32>> {
        self.get_vec_u32_property(path, LED_IFACE, "Modes").await
    }

    pub async fn get_led_color(&self, path: &str) -> Result<(u32, u32, u32)> {
        let val = self.get_property(path, LED_IFACE, "Color").await?;
        let inner: Value<'_> = val.into();
        if let Value::Structure(s) = &inner {
            if let [Value::U32(r), Value::U32(g), Value::U32(b)] = s.fields() {
                return Ok((*r, *g, *b));
            }
        }
        Err(anyhow!("Malformed Color property at {}", path))
    }

    pub async fn get_led_brightness(&self, path: &str) -> Result<u32> {
        self.get_u32_property(path, LED_IFACE, "Brightness").await
    }

    pub async fn get_led_effect_duration(&self, path: &str) -> Result<u32> {
        self.get_u32_property(path, LED_IFACE, "EffectDuration").await
    }

    pub async fn set_led_effect_duration(&self, path: &str, duration: u32) -> Result<()> {
        self.set_property(path, LED_IFACE, "EffectDuration", Value::from(duration))
            .await
    }

    pub async fn get_led_secondary_color(&self, path: &str) -> Result<(u32, u32, u32)> {
        let val = self.get_property(path, LED_IFACE, "SecondaryColor").await?;
        let inner: Value<'_> = val.into();
        if let Value::Structure(s) = &inner {
            if let [Value::U32(r), Value::U32(g), Value::U32(b)] = s.fields() {
                return Ok((*r, *g, *b));
            }
        }
        Err(anyhow!("Malformed SecondaryColor property at {}", path))
    }

    pub async fn set_led_secondary_color(&self, path: &str, r: u32, g: u32, b: u32) -> Result<()> {
        validate_rgb(r, g, b)?;
        self.set_property(path, LED_IFACE, "SecondaryColor", Value::from((r, g, b)))
            .await
    }

    pub async fn get_led_tertiary_color(&self, path: &str) -> Result<(u32, u32, u32)> {
        let val = self.get_property(path, LED_IFACE, "TertiaryColor").await?;
        let inner: Value<'_> = val.into();
        if let Value::Structure(s) = &inner {
            if let [Value::U32(r), Value::U32(g), Value::U32(b)] = s.fields() {
                return Ok((*r, *g, *b));
            }
        }
        Err(anyhow!("Malformed TertiaryColor property at {}", path))
    }

    pub async fn set_led_tertiary_color(&self, path: &str, r: u32, g: u32, b: u32) -> Result<()> {
        validate_rgb(r, g, b)?;
        self.set_property(path, LED_IFACE, "TertiaryColor", Value::from((r, g, b)))
            .await
    }

    pub async fn get_led_color_depth(&self, path: &str) -> Result<u32> {
        self.get_u32_property(path, LED_IFACE, "ColorDepth").await
    }

    pub async fn set_led_mode(&self, path: &str, mode: u32) -> Result<()> {
        self.set_property(path, LED_IFACE, "Mode", Value::from(mode)).await
    }

    pub async fn set_led_color(&self, path: &str, r: u32, g: u32, b: u32) -> Result<()> {
        validate_rgb(r, g, b)?;
        self.set_property(path, LED_IFACE, "Color", Value::from((r, g, b)))
            .await
    }

    pub async fn set_led_brightness(&self, path: &str, brightness: u32) -> Result<()> {
        anyhow::ensure!(brightness <= 255, "Brightness out of range: {} (expected 0..=255)", brightness);
        self.set_property(path, LED_IFACE, "Brightness", Value::from(brightness))
            .await
    }

    // -----------------------------------------------------------------------
    // Generic helpers
    // -----------------------------------------------------------------------

    async fn get_property(&self, path: &str, iface: &str, prop: &str) -> Result<OwnedValue> {
        let reply = self
            .conn
            .call_method(
                Some(BUS_NAME),
                path,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(iface, prop),
            )
            .await
            .with_context(|| format!("Get {}.{} at {} failed", iface, prop, path))?;
        let val: OwnedValue = reply.body().deserialize()?;
        Ok(val)
    }

    async fn set_property(&self, path: &str, iface: &str, prop: &str, value: Value<'_>) -> Result<()> {
        self.conn
            .call_method(
                Some(BUS_NAME),
                path,
                Some("org.freedesktop.DBus.Properties"),
                "Set",
                &(iface, prop, value),
            )
            .await
            .with_context(|| format!("Set {}.{} at {} failed", iface, prop, path))?;
        Ok(())
    }

    async fn get_string_property(&self, path: &str, iface: &str, prop: &str) -> Result<String> {
        let val = self.get_property(path, iface, prop).await?;
        val.downcast_ref::<String>()
            .with_context(|| format!("Type mismatch for {}.{} at {}", iface, prop, path))
    }

    async fn get_u32_property(&self, path: &str, iface: &str, prop: &str) -> Result<u32> {
        let val = self.get_property(path, iface, prop).await?;
        val.downcast_ref::<u32>()
            .with_context(|| format!("Type mismatch for {}.{} at {}", iface, prop, path))
    }

    async fn get_i32_property(&self, path: &str, iface: &str, prop: &str) -> Result<i32> {
        let val = self.get_property(path, iface, prop).await?;
        val.downcast_ref::<i32>()
            .with_context(|| format!("Type mismatch for {}.{} at {}", iface, prop, path))
    }

    async fn get_bool_property(&self, path: &str, iface: &str, prop: &str) -> Result<bool> {
        let val = self.get_property(path, iface, prop).await?;
        val.downcast_ref::<bool>()
            .with_context(|| format!("Type mismatch for {}.{} at {}", iface, prop, path))
    }

    async fn get_vec_u32_property(&self, path: &str, iface: &str, prop: &str) -> Result<Vec<u32>> {
        let val = self.get_property(path, iface, prop).await?;
        extract_u32_array(val).with_context(|| format!("Type mismatch for {}.{} at {}", iface, prop, path))
    }
}

// ---------------------------------------------------------------------------
// Free-standing helpers for extracting arrays from OwnedValue
// ---------------------------------------------------------------------------

/// Extract a `Vec<String>` of object-path strings from an `OwnedValue`
/// that wraps an array of object-paths.
fn extract_object_path_array(val: OwnedValue) -> Result<Vec<String>> {
    let inner: Value<'_> = val.into();
    match inner {
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr.iter() {
                match item {
                    Value::ObjectPath(p) => out.push(p.to_string()),
                    _ => return Err(anyhow!("Array contains non-object-path value")),
                }
            }
            Ok(out)
        }
        _ => Err(anyhow!("Value is not an array of object paths")),
    }
}

/// Extract a `Vec<u32>` from an `OwnedValue` that wraps an array of u32.
fn extract_u32_array(val: OwnedValue) -> Result<Vec<u32>> {
    let inner: Value<'_> = val.into();
    match inner {
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for value in arr.iter() {
                if let Value::U32(number) = value {
                    out.push(*number);
                } else {
                    return Err(anyhow!("Array contains non-u32 value"));
                }
            }
            Ok(out)
        }
        _ => Err(anyhow!("Value is not an array of u32")),
    }
}

fn validate_rgb(r: u32, g: u32, b: u32) -> Result<()> {
    anyhow::ensure!(r <= 255, "Red component out of range: {}", r);
    anyhow::ensure!(g <= 255, "Green component out of range: {}", g);
    anyhow::ensure!(b <= 255, "Blue component out of range: {}", b);
    Ok(())
}
