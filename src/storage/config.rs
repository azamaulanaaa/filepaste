use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::s3::{
    DEFAULT_MULTIPART_PART_SIZE_MB, DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS, S3UploadMode,
};

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
        #[serde(default)]
        s3: S3Options,
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

/// Options for the S3 storage type.
#[derive(Serialize, Deserialize, Debug)]
pub struct S3Options {
    #[serde(default)]
    pub upload_mode: S3UploadMode,
    #[serde(default = "default_multipart_part_size_mb")]
    pub multipart_part_size_mb: u64,
}

impl Default for S3Options {
    fn default() -> Self {
        Self {
            upload_mode: S3UploadMode::default(),
            multipart_part_size_mb: DEFAULT_MULTIPART_PART_SIZE_MB,
        }
    }
}

fn default_multipart_upload_expiry_hours() -> u64 {
    DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS
}

fn default_multipart_part_size_mb() -> u64 {
    DEFAULT_MULTIPART_PART_SIZE_MB
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_s3(json: &str) -> S3Options {
        let config: StorageConfig = serde_json::from_str(json).expect("S3 config");
        match config {
            StorageConfig::S3 { s3, .. } => s3,
            _ => panic!("Expected an S3 storage config"),
        }
    }

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

    #[test]
    fn s3_config_defaults_upload_mode_to_prefer_single() {
        let options = parse_s3(r#"{"type":"s3","bucket":"b","region":"r"}"#);
        assert_eq!(options.upload_mode, S3UploadMode::PreferSingle);
    }

    #[test]
    fn s3_config_parses_each_upload_mode() {
        let cases = [
            ("prefer_single", S3UploadMode::PreferSingle),
            ("require_single", S3UploadMode::RequireSingle),
            ("multipart", S3UploadMode::Multipart),
        ];

        for (mode, expected) in cases {
            let json = format!(
                r#"{{"type":"s3","bucket":"b","region":"r","s3":{{"upload_mode":"{mode}"}}}}"#
            );
            assert_eq!(parse_s3(&json).upload_mode, expected);
        }
    }

    #[test]
    fn s3_config_defaults_multipart_part_size_to_8_mib() {
        let options = parse_s3(r#"{"type":"s3","bucket":"b","region":"r"}"#);
        assert_eq!(options.multipart_part_size_mb, 8);
    }

    #[test]
    fn s3_config_parses_custom_multipart_part_sizes() {
        for size in [5, 16] {
            let json = format!(
                r#"{{"type":"s3","bucket":"b","region":"r","s3":{{"multipart_part_size_mb":{size}}}}}"#
            );
            assert_eq!(parse_s3(&json).multipart_part_size_mb, size);
        }
    }
}
