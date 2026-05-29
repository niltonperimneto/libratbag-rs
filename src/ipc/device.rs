/* DBus Device interface: per-mouse object exposing model/name/firmware and child profile paths,
 * backed by shared DeviceInfo and optional actor handle for commit/shutdown. */
use std::sync::Arc;

use tokio::sync::RwLock;
use zbus::interface;
use zbus::zvariant::ObjectPath;

use crate::engine::actor::ActorHandle;
use crate::engine::device::DeviceInfo;

use super::profile::RatbagProfile;

/// The `org.freedesktop.ratbag1.Device` interface.
///
/// Each connected mouse has one Device object registered on the DBus bus.
/// Holds a shared reference to [`DeviceInfo`] so that child objects
/// (profiles, buttons, etc.) mutate the same state that `commit()` reads.
pub struct RatbagDevice {
    info: Arc<RwLock<DeviceInfo>>,
    path: String,
    actor: Option<ActorHandle>,
}

impl RatbagDevice {
    pub fn new(info: Arc<RwLock<DeviceInfo>>, path: String, actor: Option<ActorHandle>) -> Self {
        Self { info, path, actor }
    }
}

#[interface(name = "org.freedesktop.ratbag1.Device")]
impl RatbagDevice {
    /// Device model string, e.g. "usb:046d:c539:0".
    #[zbus(property)]
    async fn model(&self) -> String {
        self.info.read().await.model.clone()
    }

    /// Human-readable device name.
    #[zbus(property)]
    async fn name(&self) -> String {
        self.info.read().await.name.clone()
    }

    /// Firmware version string, may be empty.
    #[zbus(property)]
    async fn firmware_version(&self) -> String {
        self.info.read().await.firmware_version.clone()
    }

    /// Device type: 0=unspecified, 1=other, 2=mouse, 3=keyboard.
    #[zbus(property)]
    async fn device_type(&self) -> u32 {
        self.info.read().await.device_type
    }

    /// Array of object paths to this device's profiles.
    #[zbus(property)]
    async fn profiles(&self) -> Vec<ObjectPath<'static>> {
        let info = self.info.read().await;
        info.profiles
            .iter()
            .filter_map(|p| {
                ObjectPath::try_from(format!("{}/p{}", self.path, p.index)).ok()
            })
            .collect()
    }

    /// Commit pending changes to the device hardware.
    ///
    /// Returns 0 on success. On failure, the `Resync` signal is emitted.
    /// After a successful commit the actor clears all dirty flags; we then
    /// emit `PropertiesChanged` for `IsDirty` on each profile so that
    /// listening frontends (Piper, ratbagctl) see the updated state
    /// without having to poll or restart.
    async fn commit(
        &self,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> u32 {
        let Some(ref actor) = self.actor else {
            tracing::warn!("Commit requested but no driver actor for {}", self.path);
            return 1;
        };

        match actor.commit().await {
            Ok(()) => {
                tracing::info!("Commit succeeded for {}", self.path);

                /* Notify frontends that dirty flags have been cleared. */
                let info = self.info.read().await;
                for prof in &info.profiles {
                    let path = format!("{}/p{}", self.path, prof.index);
                    if let Ok(iface_ref) =
                        server.interface::<_, RatbagProfile>(path.as_str()).await
                    {
                        let _ = iface_ref
                            .get()
                            .await
                            .is_dirty_changed(iface_ref.signal_emitter())
                            .await;
                    }
                }

                0
            }
            Err(e) => {
                tracing::error!("Commit failed for {}: {e}", self.path);
                let _ = Self::resync(&emitter).await;
                1
            }
        }
    }

    /// Signal emitted when an error occurs during commit.
    #[zbus(signal)]
    async fn resync(signal_emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}
