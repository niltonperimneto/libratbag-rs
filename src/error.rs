/* Shared ratbagd error definitions: RatbagError aggregates device/capability/value/system/DBus
 * failures for callers that need a single error type. */
use thiserror::Error;

/* Errors that may occur in ratbagd-rs. */
#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum RatbagError {
    #[error("Device error: {0}")]
    Device(String),

    #[error("Unsupported capability: {0}")]
    Capability(String),

    #[error("Invalid value: {0}")]
    Value(String),

    #[error("System error: {0}")]
    System(#[from] std::io::Error),

    #[error("DBus error: {0}")]
    Dbus(#[from] zbus::Error),

    #[error("Parse error: malformed hardware packet")]
    Parse,
}
