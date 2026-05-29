/* DBus Resolution interface: per-resolution object for DPI values, capabilities, active/default
 * flags; mutates DeviceInfo and optionally triggers hardware commit via actor. */
use std::sync::Arc;

use tokio::sync::RwLock;
use zbus::interface;
use zbus::zvariant::{OwnedValue, Value};

use crate::engine::device::{DeviceInfo, Dpi, RATBAG_RESOLUTION_CAP_SEPARATE_XY_RESOLUTION};

use super::fallback_owned_value;

/// The `org.freedesktop.ratbag1.Resolution` interface.
///
/// Represents one resolution preset within a profile.
/// State is shared with the parent device through `Arc<RwLock<DeviceInfo>>`
/// so that mutations here are visible to `commit()`.
/// Items are looked up by their stored `.index` ID, not by vector position.
pub struct RatbagResolution {
    device_info: Arc<RwLock<DeviceInfo>>,
    device_path: String,
    profile_id: u32,
    resolution_id: u32,
}

impl RatbagResolution {
    pub fn new(
        device_info: Arc<RwLock<DeviceInfo>>,
        device_path: String,
        profile_id: u32,
        resolution_id: u32,
    ) -> Self {
        Self {
            device_info,
            device_path,
            profile_id,
            resolution_id,
        }
    }

    /* Extract a u32 from a Value, accepting multiple integer types. */
    fn extract_u32(v: &Value<'_>) -> Option<u32> {
        match v {
            Value::U32(n) => Some(*n),
            Value::I32(n) => u32::try_from(*n).ok(),
            Value::U16(n) => Some(u32::from(*n)),
            Value::I16(n) => u32::try_from(*n).ok(),
            Value::U8(n) => Some(u32::from(*n)),
            Value::I64(n) => u32::try_from(*n).ok(),
            Value::U64(n) => u32::try_from(*n).ok(),
            _ => None,
        }
    }

    /* Parse a DBus Value into a DPI, handling nested variants and */
    /* multiple integer types for maximum client compatibility.    */
    fn parse_dpi_value(value: &Value<'_>) -> Option<Dpi> {
        /* Unwrap nested variant layers (property type is `v`, so     */
        /* clients may double-wrap: Properties.Set sends (ssv) where  */
        /* v contains v containing the actual value).                 */
        let unwrapped: &Value<'_> = match value {
            Value::Value(inner) => inner.as_ref(),
            other => other,
        };

        if let Some(val) = Self::extract_u32(unwrapped) {
            return Some(Dpi::Unified(val));
        }

        if let Value::Structure(s) = unwrapped {
            let fields = s.fields();
            if fields.len() == 2 {
                if let (Some(x), Some(y)) =
                    (Self::extract_u32(&fields[0]), Self::extract_u32(&fields[1]))
                {
                    return Some(Dpi::Separate { x, y });
                }
            }
        }

        None
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
    async fn set_is_disabled(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        disabled: bool,
    ) -> zbus::Result<()> {
        {
            let mut info = self.device_info.write().await;
            let profile = info.find_profile_mut(self.profile_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Profile {} not found", self.profile_id
                ))
            })?;
            let res = profile.find_resolution_mut(self.resolution_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Resolution {} not found in profile {}",
                    self.resolution_id, self.profile_id
                ))
            })?;
            res.is_disabled = disabled;
            profile.is_dirty = true;
        }
        let _ = self.is_disabled_changed(&emitter).await;
        Ok(())
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
    async fn set_resolution(&self, value: OwnedValue) -> zbus::Result<()> {
        /* Parse the incoming value before taking the write lock to minimize hold time.
         * Piper and other clients may send the DPI as a plain u32, a (u32, u32)
         * tuple, or wrapped in an extra variant layer (when the property type is `v`). */
        let inner: Value<'_> = value.into();
        let new_dpi = Self::parse_dpi_value(&inner).ok_or_else(|| {
            zbus::fdo::Error::InvalidArgs(format!(
                "Invalid resolution value: {inner:?}"
            ))
        })?;

        let mut info = self.device_info.write().await;
        let profile = info.find_profile_mut(self.profile_id).ok_or_else(|| {
            zbus::fdo::Error::Failed(format!(
                "Profile {} not found", self.profile_id
            ))
        })?;
        let res = profile.find_resolution_mut(self.resolution_id).ok_or_else(|| {
            zbus::fdo::Error::Failed(format!(
                "Resolution {} not found in profile {}",
                self.resolution_id, self.profile_id
            ))
        })?;

        /* Reject (x, y) tuples when the device lacks the SEPARATE_XY capability. */
        if matches!(new_dpi, Dpi::Separate { .. })
            && !res.capabilities.contains(&RATBAG_RESOLUTION_CAP_SEPARATE_XY_RESOLUTION)
        {
            return Err(zbus::fdo::Error::InvalidArgs(
                "Device does not support separate X/Y resolution".to_string(),
            ).into());
        }

        res.dpi = new_dpi;
        profile.is_dirty = true;
        Ok(())
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
    /// Emits `PropertiesChanged` for `IsActive` on every sibling so that
    /// frontends (Piper) update their UI without a restart.
    /// Returns 0 on success (matching the C daemon's reply signature).
    async fn set_active(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<u32> {
        let sibling_count;
        {
            let mut info = self.device_info.write().await;
            let profile = info.find_profile_mut(self.profile_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Profile {} not found", self.profile_id
                ))
            })?;
            sibling_count = profile.resolutions.len();
            for res in &mut profile.resolutions {
                res.is_active = false;
            }
            let res = profile.find_resolution_mut(self.resolution_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Resolution {} not found in profile {}",
                    self.resolution_id, self.profile_id
                ))
            })?;
            res.is_active = true;
            profile.is_dirty = true;
        }

        /* Emit IsActive changed on every sibling resolution.  This mirrors */
        /* the C daemon's ratbagd_for_each_resolution_signal callback.      */
        for i in 0..sibling_count as u32 {
            let path = format!("{}/p{}/r{}", self.device_path, self.profile_id, i);
            if let Ok(iface_ref) =
                server.interface::<_, RatbagResolution>(path.as_str()).await
            {
                let _ = iface_ref
                    .get()
                    .await
                    .is_active_changed(iface_ref.signal_emitter())
                    .await;
            }
        }

        tracing::info!(
            "Resolution {} in profile {} set as active",
            self.resolution_id,
            self.profile_id,
        );
        Ok(0)
    }

    /// Set this resolution as the default one.
    ///
    /// Clears default on all sibling resolutions in the same profile first.
    /// Emits `PropertiesChanged` for `IsDefault` on every sibling so that
    /// frontends (Piper) update their UI without a restart.
    /// Returns 0 on success (matching the C daemon's reply signature).
    async fn set_default(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<u32> {
        let sibling_count;
        {
            let mut info = self.device_info.write().await;
            let profile = info.find_profile_mut(self.profile_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Profile {} not found", self.profile_id
                ))
            })?;
            sibling_count = profile.resolutions.len();
            for res in &mut profile.resolutions {
                res.is_default = false;
            }
            let res = profile.find_resolution_mut(self.resolution_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Resolution {} not found in profile {}",
                    self.resolution_id, self.profile_id
                ))
            })?;
            res.is_default = true;
            profile.is_dirty = true;
        }

        /* Emit IsDefault changed on every sibling resolution.  This mirrors */
        /* the C daemon's ratbagd_for_each_resolution_signal callback.       */
        for i in 0..sibling_count as u32 {
            let path = format!("{}/p{}/r{}", self.device_path, self.profile_id, i);
            if let Ok(iface_ref) =
                server.interface::<_, RatbagResolution>(path.as_str()).await
            {
                let _ = iface_ref
                    .get()
                    .await
                    .is_default_changed(iface_ref.signal_emitter())
                    .await;
            }
        }

        tracing::info!(
            "Resolution {} in profile {} set as default",
            self.resolution_id,
            self.profile_id,
        );
        Ok(0)
    }
}
