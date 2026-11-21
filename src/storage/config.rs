use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StorageConfig {
    Local {
        root: PathBuf,
    },
    S3 {
        bucket: String,
        region: String,
        #[serde(default)]
        key_prefix: String,
        #[serde(default)]
        endpoint_url: Option<String>,
    },
    #[cfg(test)]
    InMemory,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self::Local {
            root: "./data".into(),
        }
    }
}
