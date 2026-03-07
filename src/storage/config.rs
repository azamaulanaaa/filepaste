use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct StorageConfig {
    pub root: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            root: "./data".into(),
        }
    }
}
