use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::s3::DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS;

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
        #[serde(default = "default_multipart_upload_expiry_hours")]
        multipart_upload_expiry_hours: u64,
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

fn default_multipart_upload_expiry_hours() -> u64 {
    DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_config_defaults_multipart_upload_expiry_to_24_hours() {
        let config: StorageConfig =
            serde_json::from_str(r#"{"type":"s3","bucket":"test-bucket","region":"us-east-1"}"#)
                .expect("S3 config");

        match config {
            StorageConfig::S3 {
                multipart_upload_expiry_hours,
                ..
            } => assert_eq!(multipart_upload_expiry_hours, 24),
            _ => panic!("Expected an S3 storage config"),
        }
    }
}
