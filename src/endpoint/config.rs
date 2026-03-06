use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EndpointConfig {
    pub port: u16,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self { port: 3000 }
    }
}
