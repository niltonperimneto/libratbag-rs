/* DBus Profile interface: exposes per-profile properties (name, rate, angle snapping, debounce,
 * resolutions/buttons/leds lists) backed by shared DeviceInfo and optional actor commit hook. */
use std::sync::Arc;

use tokio::sync::RwLock;
use zbus::interface;
use zbus::zvariant::ObjectPath;

use crate::device::DeviceInfo;

/// The `org.freedesktop.ratbag1.Profile` interface.
///
/// Represents one of a device's configurable profiles, containing
/// resolutions, buttons, and LEDs.
///
/// State is shared with the parent device through `Arc<RwLock<DeviceInfo>>`
/// so that mutations here are visible to `commit()`.
/// Items are looked up by their stored `.index` ID, not by vector position,
/// guarding against non-contiguous or reordered indices.
pub struct RatbagProfile {
    device_info: Arc<RwLock<DeviceInfo>>,
    device_path: String,
    profile_id: u32,
}

impl RatbagProfile {
    pub fn new(
        device_info: Arc<RwLock<DeviceInfo>>,
        device_path: String,
        profile_id: u32,
    ) -> Self {
        Self {
            device_info,
            device_path,
            profile_id,
        }
    }
}

#[interface(name = "org.freedesktop.ratbag1.Profile")]
impl RatbagProfile {
    /// Zero-based profile index (constant).
    #[zbus(property)]
    fn index(&self) -> u32 {
        self.profile_id
    }

    /// Profile name (read-write). Empty string means name cannot be changed.
    #[zbus(property)]
    async fn name(&self) -> String {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    #[zbus(property)]
    async fn set_name(&self, name: String) {
        let mut info = self.device_info.write().await;
        if let Some(profile) = info.find_profile_mut(self.profile_id) {
            profile.name = name;
            profile.is_dirty = true;
        }
    }

    /// True if this profile is disabled.
    #[zbus(property)]
    async fn disabled(&self) -> bool {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .is_some_and(|p| !p.is_enabled)
    }

    #[zbus(property)]
    async fn set_disabled(&self, disabled: bool) {
        let mut info = self.device_info.write().await;
        if let Some(profile) = info.find_profile_mut(self.profile_id) {
            profile.is_enabled = !disabled;
            profile.is_dirty = true;
        }
    }

    /// True if this is the active profile (read-only).
    #[zbus(property)]
    async fn is_active(&self) -> bool {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .is_some_and(|p| p.is_active)
    }

    /// True if this profile has uncommitted changes.
    #[zbus(property)]
    async fn is_dirty(&self) -> bool {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .is_some_and(|p| p.is_dirty)
    }

    /// Object paths to this profile's resolutions.
    #[zbus(property)]
    async fn resolutions(&self) -> Vec<ObjectPath<'static>> {
        let info = self.device_info.read().await;
        let Some(profile) = info.find_profile(self.profile_id) else {
            return Vec::new();
        };
        profile
            .resolutions
            .iter()
            .filter_map(|r| {
                ObjectPath::try_from(format!(
                    "{}/p{}/r{}",
                    self.device_path, self.profile_id, r.index
                ))
                .ok()
            })
            .collect()
    }

    /// Object paths to this profile's buttons.
    #[zbus(property)]
    async fn buttons(&self) -> Vec<ObjectPath<'static>> {
        let info = self.device_info.read().await;
        let Some(profile) = info.find_profile(self.profile_id) else {
            return Vec::new();
        };
        profile
            .buttons
            .iter()
            .filter_map(|b| {
                ObjectPath::try_from(format!(
                    "{}/p{}/b{}",
                    self.device_path, self.profile_id, b.index
                ))
                .ok()
            })
            .collect()
    }

    /// Object paths to this profile's LEDs.
    #[zbus(property)]
    async fn leds(&self) -> Vec<ObjectPath<'static>> {
        let info = self.device_info.read().await;
        let Some(profile) = info.find_profile(self.profile_id) else {
            return Vec::new();
        };
        profile
            .leds
            .iter()
            .filter_map(|l| {
                ObjectPath::try_from(format!(
                    "{}/p{}/l{}",
                    self.device_path, self.profile_id, l.index
                ))
                .ok()
            })
            .collect()
    }

    /// Sensor angle snapping (-1 = unsupported, 0 = off, 1 = on).
    #[zbus(property)]
    async fn angle_snapping(&self) -> i32 {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .map(|p| p.angle_snapping)
            .unwrap_or(-1)
    }

    #[zbus(property)]
    async fn set_angle_snapping(&self, value: i32) {
        let mut info = self.device_info.write().await;
        if let Some(profile) = info.find_profile_mut(self.profile_id) {
            profile.angle_snapping = value;
            profile.is_dirty = true;
        }
    }

    /// Button debounce time in ms (-1 = unsupported).
    #[zbus(property)]
    async fn debounce(&self) -> i32 {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .map(|p| p.debounce)
            .unwrap_or(-1)
    }

    #[zbus(property)]
    async fn set_debounce(&self, value: i32) {
        let mut info = self.device_info.write().await;
        if let Some(profile) = info.find_profile_mut(self.profile_id) {
            profile.debounce = value;
            profile.is_dirty = true;
        }
    }

    /// Permitted debounce time values.
    #[zbus(property)]
    async fn debounces(&self) -> Vec<u32> {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .map(|p| p.debounces.clone())
            .unwrap_or_default()
    }

    /// Report rate in Hz.
    #[zbus(property)]
    async fn report_rate(&self) -> u32 {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .map(|p| p.report_rate)
            .unwrap_or(0)
    }

    #[zbus(property)]
    async fn set_report_rate(&self, rate: u32) {
        let mut info = self.device_info.write().await;
        if let Some(profile) = info.find_profile_mut(self.profile_id) {
            profile.report_rate = rate;
            profile.is_dirty = true;
        }
    }

    /// Permitted report rate values.
    #[zbus(property)]
    async fn report_rates(&self) -> Vec<u32> {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .map(|p| p.report_rates.clone())
            .unwrap_or_default()
    }

    /// Set this profile as the active profile.
    ///
    /// Deactivates all other profiles on the same device first.
    async fn set_active(&self) {
        let mut info = self.device_info.write().await;
        for profile in &mut info.profiles {
            profile.is_active = false;
        }
        if let Some(profile) = info.find_profile_mut(self.profile_id) {
            profile.is_active = true;
            profile.is_dirty = true;
        }
        tracing::info!("Profile {} set as active", self.profile_id);
    }
}
