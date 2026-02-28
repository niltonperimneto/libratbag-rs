/* DBus Resolution interface: per-resolution object for DPI values, capabilities, active/default
 * flags; mutates DeviceInfo and optionally triggers hardware commit via actor. */
use std::sync::Arc;

use tokio::sync::RwLock;
use zbus::interface;
use zbus::zvariant::{OwnedValue, Value};

use crate::device::{DeviceInfo, Dpi};

use super::fallback_owned_value;

/// The `org.freedesktop.ratbag1.Resolution` interface.
///
/// Represents one resolution preset within a profile.
/// State is shared with the parent device through `Arc<RwLock<DeviceInfo>>`
/// so that mutations here are visible to `commit()`.
/// Items are looked up by their stored `.index` ID, not by vector position.
pub struct RatbagResolution {
    device_info: Arc<RwLock<DeviceInfo>>,
    profile_id: u32,
    resolution_id: u32,
}

impl RatbagResolution {
    pub fn new(
        device_info: Arc<RwLock<DeviceInfo>>,
        profile_id: u32,
        resolution_id: u32,
    ) -> Self {
        Self {
            device_info,
            profile_id,
            resolution_id,
        }
    }
}

#[interface(name = "org.freedesktop.ratbag1.Resolution")]
impl RatbagResolution {
    /// Zero-based resolution index (constant).
    #[zbus(property)]
    fn index(&self) -> u32 {
        self.resolution_id
    }

    /// Resolution capabilities (constant).
    #[zbus(property)]
    async fn capabilities(&self) -> Vec<u32> {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_resolution(self.resolution_id))
            .map(|r| r.capabilities.clone())
            .unwrap_or_default()
    }

    /// Whether this is the active resolution (read-only).
    #[zbus(property)]
    async fn is_active(&self) -> bool {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_resolution(self.resolution_id))
            .is_some_and(|r| r.is_active)
    }

    /// Whether this is the default resolution (read-only).
    #[zbus(property)]
    async fn is_default(&self) -> bool {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_resolution(self.resolution_id))
            .is_some_and(|r| r.is_default)
    }

    /// Whether this resolution is disabled (read-write).
    #[zbus(property)]
    async fn is_disabled(&self) -> bool {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_resolution(self.resolution_id))
            .is_some_and(|r| r.is_disabled)
    }

    #[zbus(property)]
    async fn set_is_disabled(&self, disabled: bool) {
        let mut info = self.device_info.write().await;
        if let Some(profile) = info.find_profile_mut(self.profile_id) {
            if let Some(res) = profile.find_resolution_mut(self.resolution_id) {
                res.is_disabled = disabled;
            }
            profile.is_dirty = true;
        }
    }

    /// DPI value as a variant: either a `u32` or a `(u32, u32)` tuple.
    #[zbus(property)]
    async fn resolution(&self) -> OwnedValue {
        let info = self.device_info.read().await;
        let dpi = info
            .find_profile(self.profile_id)
            .and_then(|p| p.find_resolution(self.resolution_id))
            .map(|r| r.dpi)
            .unwrap_or(Dpi::Unknown);
        match dpi {
            Dpi::Unified(val) => {
                OwnedValue::try_from(Value::from(val)).unwrap_or_else(|_| fallback_owned_value())
            }
            Dpi::Separate { x, y } => {
                OwnedValue::try_from(Value::from((x, y)))
                    .unwrap_or_else(|_| fallback_owned_value())
            }
            Dpi::Unknown => fallback_owned_value(),
        }
    }

    #[zbus(property)]
    async fn set_resolution(&self, value: OwnedValue) {
        // Parse the incoming value before taking the write lock to minimize hold time.
        let inner: Value<'_> = value.into();
        let new_dpi = match &inner {
            Value::U32(val) => Some(Dpi::Unified(*val)),
            Value::Structure(s) => {
                if let [Value::U32(x), Value::U32(y)] = s.fields() {
                    Some(Dpi::Separate { x: *x, y: *y })
                } else {
                    tracing::warn!("Invalid structure in resolution value");
                    None
                }
            }
            _ => {
                tracing::warn!("Invalid resolution value received over DBus");
                None
            }
        };

        if let Some(dpi) = new_dpi {
            let mut info = self.device_info.write().await;
            if let Some(profile) = info.find_profile_mut(self.profile_id) {
                if let Some(res) = profile.find_resolution_mut(self.resolution_id) {
                    res.dpi = dpi;
                }
                profile.is_dirty = true;
            }
        }
    }

    /// List of supported DPI values (constant).
    #[zbus(property)]
    async fn resolutions(&self) -> Vec<u32> {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_resolution(self.resolution_id))
            .map(|r| r.dpi_list.clone())
            .unwrap_or_default()
    }

    /// Set this resolution as the active one.
    ///
    /// Deactivates all sibling resolutions in the same profile first.
    async fn set_active(&self) {
        let mut info = self.device_info.write().await;
        if let Some(profile) = info.find_profile_mut(self.profile_id) {
            for res in &mut profile.resolutions {
                res.is_active = false;
            }
            if let Some(res) = profile.find_resolution_mut(self.resolution_id) {
                res.is_active = true;
            }
            profile.is_dirty = true;
            tracing::info!(
                "Resolution {} in profile {} set as active",
                self.resolution_id,
                self.profile_id,
            );
        }
    }

    /// Set this resolution as the default one.
    ///
    /// Clears default on all sibling resolutions in the same profile first.
    async fn set_default(&self) {
        let mut info = self.device_info.write().await;
        if let Some(profile) = info.find_profile_mut(self.profile_id) {
            for res in &mut profile.resolutions {
                res.is_default = false;
            }
            if let Some(res) = profile.find_resolution_mut(self.resolution_id) {
                res.is_default = true;
            }
            profile.is_dirty = true;
            tracing::info!(
                "Resolution {} in profile {} set as default",
                self.resolution_id,
                self.profile_id,
            );
        }
    }
}
