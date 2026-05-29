/* DBus Button interface: exposes per-button action type/mapping and supported actions, updating the
 * shared DeviceInfo and delegating commits through the device actor when needed. */
use std::sync::Arc;

use tokio::sync::RwLock;
use zbus::interface;
use zbus::zvariant::{OwnedValue, Value};

use crate::engine::device::{ActionType, DeviceInfo};

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
    None,
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
            ActionType::Button | ActionType::Special | ActionType::Key => {
                OwnedValue::try_from(Value::from(button.mapping_value))
                    .unwrap_or_else(|_| fallback_owned_value())
            }
            ActionType::None | ActionType::Unknown => {
                OwnedValue::try_from(Value::from(0_u32))
                    .unwrap_or_else(|_| fallback_owned_value())
            }
        };

        (action_type, value)
    }

    #[zbus(property)]
    async fn set_mapping(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        mapping: (u32, OwnedValue),
    ) -> zbus::Result<()> {
        let (action_type_raw, value) = mapping;
        let action_type = match action_type_raw {
            0 => ActionType::None,
            1 => ActionType::Button,
            2 => ActionType::Special,
            3 => ActionType::Key,
            4 => ActionType::Macro,
            _ => {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "Unsupported action type: {action_type_raw}"
                ))
                .into());
            }
        };

        /* Unwrap nested Variant wrappers: some DBus clients (e.g. Piper/GLib)
         * may send Value::Value(Value::U32(...)) instead of Value::U32(...). */
        let mut inner: Value<'_> = value.into();
        while let Value::Value(boxed) = inner {
            inner = *boxed;
        }

        let parsed = match action_type {
            ActionType::None => {
                if matches!(inner, Value::U32(_)) {
                    Some(ParsedMapping::None)
                } else {
                    tracing::warn!(
                        "Button {}: expected U32 for None mapping, got {:?}",
                        self.button_id,
                        inner.value_signature(),
                    );
                    None
                }
            }
            ActionType::Macro => {
                if let Value::Array(arr) = &inner {
                    let mut entries = Vec::with_capacity(arr.len());
                    for value in arr.iter() {
                        let Value::Structure(s) = value else {
                            tracing::warn!(
                                "Button {}: expected Struct(u32,u32) entries for Macro mapping",
                                self.button_id,
                            );
                            return Err(zbus::fdo::Error::InvalidArgs(
                                "Invalid macro entry type".into(),
                            )
                            .into());
                        };
                        let [Value::U32(a), Value::U32(b)] = s.fields() else {
                            tracing::warn!(
                                "Button {}: expected Struct(u32,u32) fields for Macro mapping",
                                self.button_id,
                            );
                            return Err(zbus::fdo::Error::InvalidArgs(
                                "Invalid macro entry fields".into(),
                            )
                            .into());
                        };
                        entries.push((*a, *b));
                    }
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
            ActionType::Button | ActionType::Special | ActionType::Key => {
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
            ActionType::Unknown => None,
        };

        let Some(parsed) = parsed else {
            return Err(zbus::fdo::Error::InvalidArgs(
                "Invalid mapping payload for action type".into(),
            )
            .into());
        };

        let mut mapping_value = 0;
        let mut macro_entries = Vec::new();
        match parsed {
            ParsedMapping::None => {}
            ParsedMapping::Macro(entries) => {
                macro_entries = entries;
            }
            ParsedMapping::Simple(val) => {
                mapping_value = val;
            }
        }

        {
            let mut info = self.device_info.write().await;
            let _ = info
                .find_profile(self.profile_id)
                .ok_or_else(|| zbus::fdo::Error::Failed("Profile not found".into()))?;
            let _ = info
                .find_profile(self.profile_id)
                .and_then(|p| p.find_button(self.button_id))
                .ok_or_else(|| zbus::fdo::Error::Failed("Button not found".into()))?;

            *info = info.with_button_mapping(
                self.profile_id,
                self.button_id,
                action_type,
                mapping_value,
                macro_entries,
            );
        }
        let _ = self.mapping_changed(&emitter).await;
        Ok(())
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
