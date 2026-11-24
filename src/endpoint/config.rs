use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default)]
pub struct EndpointConfig {
    pub host: String,
    pub port: u16,
    pub index_path: Option<String>,
    pub job_store_path: PathBuf,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 3000,
            index_path: None,
            job_store_path: default_job_store_path(),
        }
    }
}

/// Return the default directory for the durable job records.
fn default_job_store_path() -> PathBuf {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    if let Some(state_home) = state_home {
        return state_home.join("filepaste").join("jobs");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("filepaste")
            .join("jobs");
    }
    PathBuf::from(".filepaste").join("jobs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_the_job_store_path_for_an_old_config() {
        let config: EndpointConfig =
            serde_json::from_str(r#"{"host":"127.0.0.1","port":8080,"index_path":null}"#)
                .expect("deserialize the endpoint config");

        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 8080);
        assert_eq!(config.job_store_path, default_job_store_path());
    }
}
