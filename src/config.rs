use serde::{Deserialize, Serialize};

use crate::endpoint::config::EndpointConfig;

#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct AppConfig {
    pub endpoint: EndpointConfig,
}
