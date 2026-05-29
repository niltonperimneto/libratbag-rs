/* DBus Profile interface: exposes per-profile properties (name, rate, angle snapping, debounce,
 * capabilities, resolutions/buttons/leds lists) backed by shared DeviceInfo.
 *
 * Design principles:
 * - Validate inputs *before* acquiring the write lock to keep critical sections short.
 * - Clamp / reject invalid values using typed helpers on `ProfileInfo`.
 * - Return `zbus::fdo::Result` from setters so callers see failures.
 * - Emit `PropertiesChanged` signals for every mutated property plus `IsDirty`. */
use std::sync::Arc;

use tokio::sync::RwLock;
use zbus::interface;
use zbus::zvariant::ObjectPath;

use crate::engine::device::{DeviceInfo, ProfileInfo};

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
    // ------------------------------------------------------------------
    // Read-only constant properties
    // ------------------------------------------------------------------

    /// Zero-based profile index (constant).
    #[zbus(property)]
    fn index(&self) -> u32 {
        self.profile_id
    }

    /// Profile capabilities (constant).
    ///
    /// Returns the subset of well-known profile capabilities
    /// (`SET_DEFAULT` = 101, `DISABLE` = 102) that this profile supports,
    /// matching the C daemon's `ratbagd_profile_get_capabilities`.
    #[zbus(property)]
    async fn capabilities(&self) -> Vec<u32> {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .map(|p| p.dbus_capabilities())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // Read-write properties
    // ------------------------------------------------------------------

    /// Profile name (read-write).
    ///
    /// The getter sanitises the raw name for safe DBus transport
    /// (UTF-8 pass-through, ISO-8859-1 fallback, then ASCII-only).
    #[zbus(property)]
    async fn name(&self) -> String {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .map(|p| ProfileInfo::sanitize_name(&p.name))
            .unwrap_or_default()
    }

    #[zbus(property)]
    async fn set_name(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        name: String,
    ) -> zbus::Result<()> {
        {
            let mut info = self.device_info.write().await;
            let _ = info
                .find_profile(self.profile_id)
                .ok_or_else(|| zbus::fdo::Error::Failed("Profile not found".into()))?;
            *info = info.with_profile_name(self.profile_id, name);
        }
        let _ = self.name_changed(&emitter).await;
        let _ = self.is_dirty_changed(&emitter).await;
        Ok(())
    }

    /// True if this profile is disabled.
    #[zbus(property)]
    async fn disabled(&self) -> bool {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .is_some_and(|p| !p.is_enabled)
    }

    #[zbus(property)]
    async fn set_disabled(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        disabled: bool,
    ) -> zbus::Result<()> {
        {
            let mut info = self.device_info.write().await;
            let _ = info
                .find_profile(self.profile_id)
                .ok_or_else(|| zbus::fdo::Error::Failed("Profile not found".into()))?;
            *info = info.with_profile_disabled(self.profile_id, disabled);
        }
        let _ = self.disabled_changed(&emitter).await;
        let _ = self.is_dirty_changed(&emitter).await;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Read-only dynamic properties
    // ------------------------------------------------------------------

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

    // ------------------------------------------------------------------
    // Child object paths
    // ------------------------------------------------------------------

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

    // ------------------------------------------------------------------
    // Angle snapping / debounce / report rate
    // ------------------------------------------------------------------

    /// Sensor angle snapping (-1 = unsupported, 0 = off, 1 = on).
    #[zbus(property)]
    async fn angle_snapping(&self) -> i32 {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .map(|p| p.angle_snapping)
            .unwrap_or(-1)
    }

    #[zbus(property)]
    async fn set_angle_snapping(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        value: i32,
    ) -> zbus::Result<()> {
        {
            let mut info = self.device_info.write().await;
            let _ = info
                .find_profile(self.profile_id)
                .ok_or_else(|| zbus::fdo::Error::Failed("Profile not found".into()))?;
            *info = info.with_profile_angle_snapping(self.profile_id, value);
        }
        let _ = self.angle_snapping_changed(&emitter).await;
        let _ = self.is_dirty_changed(&emitter).await;
        Ok(())
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
    async fn set_debounce(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        value: i32,
    ) -> zbus::Result<()> {
        {
            let mut info = self.device_info.write().await;
            let _ = info
                .find_profile(self.profile_id)
                .ok_or_else(|| zbus::fdo::Error::Failed("Profile not found".into()))?;
            *info = info.with_profile_debounce(self.profile_id, value);
        }
        let _ = self.debounce_changed(&emitter).await;
        let _ = self.is_dirty_changed(&emitter).await;
        Ok(())
    }

    /// Permitted debounce time values (constant).
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

    /// Set report rate in Hz.
    ///
    /// The value is clamped to [125, 8000] before storage, matching the
    /// C daemon's sanity check.
    #[zbus(property)]
    async fn set_report_rate(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        rate: u32,
    ) -> zbus::Result<()> {
        /* Clamp *before* acquiring the write lock. */
        let clamped = ProfileInfo::clamp_report_rate(rate);

        {
            let mut info = self.device_info.write().await;
            let _ = info
                .find_profile(self.profile_id)
                .ok_or_else(|| zbus::fdo::Error::Failed("Profile not found".into()))?;
            *info = info.with_profile_report_rate(self.profile_id, clamped);
        }
        let _ = self.report_rate_changed(&emitter).await;
        let _ = self.is_dirty_changed(&emitter).await;
        Ok(())
    }

    /// Permitted report rate values (constant).
    #[zbus(property)]
    async fn report_rates(&self) -> Vec<u32> {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .map(|p| p.report_rates.clone())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // Methods
    // ------------------------------------------------------------------

    /// Set this profile as the active profile.
    ///
    /// Deactivates all other profiles on the same device first, then marks
    /// this profile as active and dirty.  Emits `PropertiesChanged` for
    /// `IsActive` on every affected profile so that listening frontends
    /// (e.g. Piper) update immediately without a restart.
    async fn set_active(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<()> {
        let old_active_id;
        {
            let mut info = self.device_info.write().await;
            old_active_id = info
                .profiles
                .iter()
                .find(|p| p.is_active)
                .map(|p| p.index);
            let _ = info
                .find_profile(self.profile_id)
                .ok_or_else(|| zbus::fdo::Error::Failed("Profile not found".into()))?;
            *info = info.with_active_profile(self.profile_id);
        }
        /* Lock released — now emit PropertiesChanged signals.         */
        /*                                                              */
        /* We notify the previously-active profile (IsActive → false)   */
        /* and the newly-active profile (IsActive → true, IsDirty →     */
        /* true).  Each signal must be emitted through the              */
        /* SignalEmitter of the *correct* object path, obtained from    */
        /* the ObjectServer's InterfaceRef for that path.               */

        if let Some(old_id) = old_active_id {
            if old_id != self.profile_id {
                let path = format!("{}/p{}", self.device_path, old_id);
                if let Ok(iface_ref) =
                    server.interface::<_, RatbagProfile>(path.as_str()).await
                {
                    let _ = iface_ref
                        .get()
                        .await
                        .is_active_changed(iface_ref.signal_emitter())
                        .await;
                }
            }
        }

        let new_path = format!("{}/p{}", self.device_path, self.profile_id);
        if let Ok(iface_ref) =
            server.interface::<_, RatbagProfile>(new_path.as_str()).await
        {
            let _ = iface_ref
                .get()
                .await
                .is_active_changed(iface_ref.signal_emitter())
                .await;
            let _ = iface_ref
                .get()
                .await
                .is_dirty_changed(iface_ref.signal_emitter())
                .await;
        }

        tracing::info!("Profile {} set as active", self.profile_id);
        Ok(())
    }
}
