use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StorageConfig {
    Local {
        root: PathBuf,
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
