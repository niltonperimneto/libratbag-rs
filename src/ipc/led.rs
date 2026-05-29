/* DBus LED interface: per-LED object managing mode, colors, brightness, and effect duration for a
 * profile LED, writing changes into DeviceInfo and committing via the actor. */
use std::sync::Arc;

use tokio::sync::RwLock;
use zbus::interface;

use crate::engine::device::{Color, DeviceInfo, LedMode};

/// The `org.freedesktop.ratbag1.Led` interface.
///
/// Represents one LED on a mouse within a given profile.
/// Supports multi-color modes (Starlight, TriColor) via secondary/tertiary colors.
/// State is shared with the parent device through `Arc<RwLock<DeviceInfo>>`
/// so that mutations here are visible to `commit()`.
/// Items are looked up by their stored `.index` ID, not by vector position.
pub struct RatbagLed {
    device_info: Arc<RwLock<DeviceInfo>>,
    profile_id: u32,
    led_id: u32,
}

impl RatbagLed {
    pub fn new(
        device_info: Arc<RwLock<DeviceInfo>>,
        profile_id: u32,
        led_id: u32,
    ) -> Self {
        Self {
            device_info,
            profile_id,
            led_id,
        }
    }
}

/// Convert a DBus RGB tuple `(u32, u32, u32)` into a [`Color`], clamping to 255.
#[inline]
fn color_from_tuple(t: (u32, u32, u32)) -> Color {
    Color {
        red: t.0.min(255),
        green: t.1.min(255),
        blue: t.2.min(255),
    }
}

/// Convert a [`Color`] into a DBus RGB tuple.
#[inline]
fn color_to_tuple(c: &Color) -> (u32, u32, u32) {
    (c.red, c.green, c.blue)
}

#[interface(name = "org.freedesktop.ratbag1.Led")]
impl RatbagLed {
    /// Zero-based LED index (constant).
    #[zbus(property)]
    fn index(&self) -> u32 {
        self.led_id
    }

    /// Current LED mode as a u32 discriminant (read-write).
    #[zbus(property)]
    async fn mode(&self) -> u32 {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_led(self.led_id))
            .map(|l| l.mode as u32)
            .unwrap_or(LedMode::Off as u32)
    }

    #[zbus(property)]
    async fn set_mode(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        mode: u32,
    ) -> zbus::Result<()> {
        let led_mode = LedMode::from_u32(mode).ok_or_else(|| {
            zbus::fdo::Error::InvalidArgs(format!("Invalid LedMode: {mode}"))
        })?;
        {
            let mut info = self.device_info.write().await;
            let profile = info.find_profile(self.profile_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Profile {} not found", self.profile_id
                ))
            })?;
            let led = profile.find_led(self.led_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Led {} not found in profile {}", self.led_id, self.profile_id
                ))
            })?;
            if !led.modes.contains(&led_mode) {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "LedMode {} not supported by this LED", mode
                )).into());
            }
            *info = info.with_led_mode(self.profile_id, self.led_id, led_mode);
        }
        let _ = self.mode_changed(&emitter).await;
        Ok(())
    }

    /// Supported LED modes as u32 discriminants (constant).
    #[zbus(property)]
    async fn modes(&self) -> Vec<u32> {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_led(self.led_id))
            .map(|l| l.modes.iter().map(|m| *m as u32).collect())
            .unwrap_or_default()
    }

    /// Primary LED color as an RGB triplet (read-write).
    #[zbus(property)]
    async fn color(&self) -> (u32, u32, u32) {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_led(self.led_id))
            .map(|l| color_to_tuple(&l.color))
            .unwrap_or_default()
    }

    #[zbus(property)]
    async fn set_color(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        color: (u32, u32, u32),
    ) -> zbus::Result<()> {
        {
            let mut info = self.device_info.write().await;
            let _ = info.find_profile(self.profile_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Profile {} not found", self.profile_id
                ))
            })?;
            let _ = info.find_profile(self.profile_id)
                .and_then(|p| p.find_led(self.led_id))
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed(format!(
                        "Led {} not found in profile {}", self.led_id, self.profile_id
                    ))
                })?;
            *info = info.with_led_color(self.profile_id, self.led_id, color_from_tuple(color));
        }
        let _ = self.color_changed(&emitter).await;
        Ok(())
    }

    /// Secondary LED color for multi-color effects like Starlight (read-write).
    #[zbus(property)]
    async fn secondary_color(&self) -> (u32, u32, u32) {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_led(self.led_id))
            .map(|l| color_to_tuple(&l.secondary_color))
            .unwrap_or_default()
    }

    #[zbus(property)]
    async fn set_secondary_color(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        color: (u32, u32, u32),
    ) -> zbus::Result<()> {
        {
            let mut info = self.device_info.write().await;
            let _ = info.find_profile(self.profile_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Profile {} not found", self.profile_id
                ))
            })?;
            let _ = info.find_profile(self.profile_id)
                .and_then(|p| p.find_led(self.led_id))
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed(format!(
                        "Led {} not found in profile {}", self.led_id, self.profile_id
                    ))
                })?;
            *info = info.with_led_secondary_color(self.profile_id, self.led_id, color_from_tuple(color));
        }
        let _ = self.secondary_color_changed(&emitter).await;
        Ok(())
    }

    /// Tertiary LED color for 3-zone effects like G203 TriColor (read-write).
    #[zbus(property)]
    async fn tertiary_color(&self) -> (u32, u32, u32) {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_led(self.led_id))
            .map(|l| color_to_tuple(&l.tertiary_color))
            .unwrap_or_default()
    }

    #[zbus(property)]
    async fn set_tertiary_color(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        color: (u32, u32, u32),
    ) -> zbus::Result<()> {
        {
            let mut info = self.device_info.write().await;
            let _ = info.find_profile(self.profile_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Profile {} not found", self.profile_id
                ))
            })?;
            let _ = info.find_profile(self.profile_id)
                .and_then(|p| p.find_led(self.led_id))
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed(format!(
                        "Led {} not found in profile {}", self.led_id, self.profile_id
                    ))
                })?;
            *info = info.with_led_tertiary_color(self.profile_id, self.led_id, color_from_tuple(color));
        }
        let _ = self.tertiary_color_changed(&emitter).await;
        Ok(())
    }

    /// Color depth enum (constant).
    #[zbus(property)]
    async fn color_depth(&self) -> u32 {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_led(self.led_id))
            .map(|l| l.color_depth)
            .unwrap_or(0)
    }

    /// Effect duration in ms, range 0-10000 (read-write).
    #[zbus(property)]
    async fn effect_duration(&self) -> u32 {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_led(self.led_id))
            .map(|l| l.effect_duration)
            .unwrap_or(0)
    }

    #[zbus(property)]
    async fn set_effect_duration(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        duration: u32,
    ) -> zbus::Result<()> {
        {
            let mut info = self.device_info.write().await;
            let _ = info.find_profile(self.profile_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Profile {} not found", self.profile_id
                ))
            })?;
            let _ = info.find_profile(self.profile_id)
                .and_then(|p| p.find_led(self.led_id))
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed(format!(
                        "Led {} not found in profile {}", self.led_id, self.profile_id
                    ))
                })?;
            *info = info.with_led_effect_duration(self.profile_id, self.led_id, duration.min(10000));
        }
        let _ = self.effect_duration_changed(&emitter).await;
        Ok(())
    }

    /// LED brightness, 0-255 (read-write).
    #[zbus(property)]
    async fn brightness(&self) -> u32 {
        let info = self.device_info.read().await;
        info.find_profile(self.profile_id)
            .and_then(|p| p.find_led(self.led_id))
            .map(|l| l.brightness)
            .unwrap_or(0)
    }

    #[zbus(property)]
    async fn set_brightness(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        brightness: u32,
    ) -> zbus::Result<()> {
        {
            let mut info = self.device_info.write().await;
            let _ = info.find_profile(self.profile_id).ok_or_else(|| {
                zbus::fdo::Error::Failed(format!(
                    "Profile {} not found", self.profile_id
                ))
            })?;
            let _ = info.find_profile(self.profile_id)
                .and_then(|p| p.find_led(self.led_id))
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed(format!(
                        "Led {} not found in profile {}", self.led_id, self.profile_id
                    ))
                })?;
            *info = info.with_led_brightness(self.profile_id, self.led_id, brightness.min(255));
        }
        let _ = self.brightness_changed(&emitter).await;
        Ok(())
    }
}
