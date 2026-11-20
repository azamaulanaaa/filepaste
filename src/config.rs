use rand::distr::{Alphanumeric, Distribution};
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
            password_salt: Alphanumeric
                .sample_iter(rand::rng())
                .take(32)
                .map(char::from)
                .collect(),
            default_retention_hours: 24,
            totp_secret: Alphanumeric
                .sample_iter(rand::rng())
                .take(32)
                .map(char::from)
                .collect(),
        }
    }
}
