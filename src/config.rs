use serde::{Deserialize, Serialize};

use crate::{endpoint::config::EndpointConfig, storage::config::StorageConfig};

#[derive(Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AppConfig {
    pub endpoint: EndpointConfig,
    pub storage: StorageConfig,
    pub password_salt: String,
    pub default_retention_hours: u64,
    pub totp_secret: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            endpoint: Default::default(),
            storage: Default::default(),
            password_salt: env!("CARGO_PKG_NAME").to_string(),
            default_retention_hours: 24,
            totp_secret: "LongLongSecret".to_string(),
        }
    }
}
