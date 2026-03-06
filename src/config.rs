use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct AppConfig {
    pub api_key: String,
    pub timeout: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            api_key: "default_key".into(),
            timeout: 30,
        }
    }
}
