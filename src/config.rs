use serde::{Deserialize, Serialize};

use crate::{endpoint::config::EndpointConfig, storage::config::StorageConfig};

#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct AppConfig {
    pub endpoint: EndpointConfig,
    pub storage: StorageConfig,
    pub password_salt: String,
}
