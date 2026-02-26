use std::sync::Arc;

use tokio::sync::RwLock;
use zbus::interface;

use crate::device::{LedInfo, LedMode};

/* The org.freedesktop.ratbag1.Led interface. */
/*  */
/* Represents one LED on a mouse within a given profile. */
/* Supports multi-color modes (Starlight, TriColor) via secondary/tertiary colors. */
pub struct RatbagLed {
    info: Arc<RwLock<LedInfo>>,
}

impl RatbagLed {
    pub fn new(info: LedInfo) -> Self {
        Self {
            info: Arc::new(RwLock::new(info)),
        }
    }
}

#[interface(name = "org.freedesktop.ratbag1.Led")]
impl RatbagLed {
    /* Zero-based LED index (constant). */
    #[zbus(property)]
    async fn index(&self) -> u32 {
        self.info.read().await.index
    }

    /* Current LED mode as a u32 discriminant (read-write). */
    #[zbus(property)]
    async fn mode(&self) -> u32 {
        self.info.read().await.mode as u32
    }

    #[zbus(property)]
    async fn set_mode(&self, mode: u32) -> zbus::Result<()> {
        if let Some(led_mode) = LedMode::from_u32(mode) {
            self.info.write().await.mode = led_mode;
            Ok(())
        } else {
            Err(zbus::fdo::Error::InvalidArgs(format!(
                "Invalid LedMode: {}",
                mode
            ))
            .into())
        }
    }

    /* Supported LED modes as u32 discriminants (constant). */
    #[zbus(property)]
    async fn modes(&self) -> Vec<u32> {
        self.info
            .read()
            .await
            .modes
            .iter()
            .map(|m| *m as u32)
            .collect()
    }

    /* Primary LED color as an RGB triplet (read-write). */
    #[zbus(property)]
    async fn color(&self) -> (u32, u32, u32) {
        let info = self.info.read().await;
        (info.color.red, info.color.green, info.color.blue)
    }

    #[zbus(property)]
    async fn set_color(&self, color: (u32, u32, u32)) {
        let mut info = self.info.write().await;
        info.color.red = color.0.min(255);
        info.color.green = color.1.min(255);
        info.color.blue = color.2.min(255);
    }

    /* Secondary LED color for multi-color effects like Starlight (read-write). */
    #[zbus(property)]
    async fn secondary_color(&self) -> (u32, u32, u32) {
        let info = self.info.read().await;
        (
            info.secondary_color.red,
            info.secondary_color.green,
            info.secondary_color.blue,
        )
    }

    #[zbus(property)]
    async fn set_secondary_color(&self, color: (u32, u32, u32)) {
        let mut info = self.info.write().await;
        info.secondary_color.red = color.0.min(255);
        info.secondary_color.green = color.1.min(255);
        info.secondary_color.blue = color.2.min(255);
    }

    /* Tertiary LED color for 3-zone effects like G203 TriColor (read-write). */
    #[zbus(property)]
    async fn tertiary_color(&self) -> (u32, u32, u32) {
        let info = self.info.read().await;
        (
            info.tertiary_color.red,
            info.tertiary_color.green,
            info.tertiary_color.blue,
        )
    }

    #[zbus(property)]
    async fn set_tertiary_color(&self, color: (u32, u32, u32)) {
        let mut info = self.info.write().await;
        info.tertiary_color.red = color.0.min(255);
        info.tertiary_color.green = color.1.min(255);
        info.tertiary_color.blue = color.2.min(255);
    }

    /* Color depth enum (constant). */
    #[zbus(property)]
    async fn color_depth(&self) -> u32 {
        self.info.read().await.color_depth
    }

    /* Effect duration in ms, range 0-10000 (read-write). */
    #[zbus(property)]
    async fn effect_duration(&self) -> u32 {
        self.info.read().await.effect_duration
    }

    #[zbus(property)]
    async fn set_effect_duration(&self, duration: u32) {
        self.info.write().await.effect_duration = duration.min(10000);
    }

    /* LED brightness, 0-255 (read-write). */
    #[zbus(property)]
    async fn brightness(&self) -> u32 {
        self.info.read().await.brightness
    }

    #[zbus(property)]
    async fn set_brightness(&self, brightness: u32) {
        self.info.write().await.brightness = brightness.min(255);
    }
}
