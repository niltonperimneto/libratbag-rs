/* DBus Button interface: exposes per-button action type/mapping and supported actions, updating the
 * shared DeviceInfo and delegating commits through the device actor when needed. */
use std::sync::Arc;

use tokio::sync::RwLock;
use zbus::interface;
use zbus::zvariant::{OwnedValue, Value};

use crate::device::{ActionType, DeviceInfo};

use super::fallback_owned_value;

/// The `org.freedesktop.ratbag1.Button` interface.
///
/// Represents one physical button on a mouse within a given profile.
/// State is shared with the parent device through `Arc<RwLock<DeviceInfo>>`
/// so that mutations here are visible to `commit()`.
/// Items are looked up by their stored `.index` ID, not by vector position.
pub struct RatbagButton {
    device_info: Arc<RwLock<DeviceInfo>>,
    profile_id: u32,
    button_id: u32,
}

impl RatbagButton {
    pub fn new(
        device_info: Arc<RwLock<DeviceInfo>>,
        profile_id: u32,
        button_id: u32,
    ) -> Self {
        Self {
            device_info,
            profile_id,
            button_id,
        }
    }
}

/// Intermediate representation for parsed button mapping values.
///
/// Allows parsing the DBus variant *before* acquiring the write lock,
/// keeping the critical section as short as possible.
enum ParsedMapping {
    Macro(Vec<(u32, u32)>),
    Simple(u32),
}

#[interface(name = "org.freedesktop.ratbag1.Button")]
impl RatbagButton {
    /// Zero-based button index (constant).
    #[zbus(property)]
    fn index(&self) -> u32 {
        self.button_id
    }

    /// Current button mapping as `(ActionType, Variant)`.
    ///
    /// `ActionType` determines the variant format:
    /// - Button (1): `u32` button number
    /// - Special (2): `u32` special value
    /// - Key (3): `u32` keycode
    /// - Macro (4): `Vec<(u32, u32)>` key events
    /// - None (0) / Unknown (1000): `u32` with value 0
    #[zbus(property)]
    async fn mapping(&self) -> (u32, OwnedValue) {
        let info = self.device_info.read().await;
        let Some(profile) = info.find_profile(self.profile_id) else {
            return (ActionType::None as u32, fallback_owned_value());
        };
        let Some(button) = profile.find_button(self.button_id) else {
            return (ActionType::None as u32, fallback_owned_value());
        };
        let action_type = button.action_type as u32;

        let value: OwnedValue = match button.action_type {
            ActionType::Macro => {
                OwnedValue::try_from(Value::from(button.macro_entries.clone()))
                    .unwrap_or_else(|_| fallback_owned_value())
            }
            _ => OwnedValue::try_from(Value::from(button.mapping_value))
                .unwrap_or_else(|_| fallback_owned_value()),
        };

        (action_type, value)
    }

    #[zbus(property)]
    async fn set_mapping(&self, mapping: (u32, OwnedValue)) {
        let (action_type_raw, value) = mapping;
        let action_type = ActionType::from_u32(action_type_raw);

        // Parse the incoming value before taking the write lock to minimize hold time.
        let inner: Value<'_> = value.into();
        let parsed = match action_type {
            ActionType::Macro => {
                if let Value::Array(arr) = &inner {
                    let entries: Vec<(u32, u32)> = arr
                        .iter()
                        .filter_map(|v| {
                            if let Value::Structure(s) = v {
                                if let [Value::U32(a), Value::U32(b)] = s.fields() {
                                    return Some((*a, *b));
                                }
                            }
                            None
                        })
                        .collect();
                    Some(ParsedMapping::Macro(entries))
                } else {
                    tracing::warn!(
                        "Button {}: expected Array for Macro mapping, got {:?}",
                        self.button_id,
                        inner.value_signature(),
                    );
                    None
                }
            }
            _ => {
                if let Value::U32(val) = &inner {
                    Some(ParsedMapping::Simple(*val))
                } else {
                    tracing::warn!(
                        "Button {}: expected U32 for {:?} mapping, got {:?}",
                        self.button_id,
                        action_type,
                        inner.value_signature(),
                    );
                    None
                }
            }
        };

        let mut info = self.device_info.write().await;
        if let Some(profile) = info.find_profile_mut(self.profile_id) {
            if let Some(button) = profile.find_button_mut(self.button_id) {
                button.action_type = action_type;
                match parsed {
                    Some(ParsedMapping::Macro(entries)) => button.macro_entries = entries,
                    Some(ParsedMapping::Simple(val)) => button.mapping_value = val,
                    None => {}
                }
            }
            profile.is_dirty = true;
        }
    }

    /// Supported action types for this button (constant).
    #[zbus(property)]
    async fn action_types(&self) -> Vec<u32> {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_button(self.button_id))
            .map(|b| b.action_types.clone())
            .unwrap_or_default()
    }
}
