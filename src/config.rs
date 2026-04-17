use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Gitea instance URL (e.g. https://git.calii.net)
    pub instance: String,
    /// Runner name
    #[serde(default = "default_name")]
    pub name: String,
    /// Runner labels (e.g. ["self-hosted:host", "macos-arm64:host"])
    #[serde(default = "default_labels")]
    pub labels: Vec<String>,
    /// Max concurrent jobs
    #[serde(default = "default_capacity")]
    pub capacity: usize,
    /// Poll interval in seconds
    #[serde(default = "default_fetch_interval")]
    pub fetch_interval: u64,
    /// Job timeout in seconds
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// Working directory for job execution
    #[serde(default = "default_work_dir")]
    pub work_dir: PathBuf,
    /// Path to .runner credentials file
    #[serde(default = "default_runner_file")]
    pub runner_file: PathBuf,
}

fn default_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "act-runner-rs".to_string())
}

fn default_labels() -> Vec<String> {
    vec!["self-hosted:host".to_string()]
}

fn default_capacity() -> usize {
    1
}

fn default_fetch_interval() -> u64 {
    2
}

fn default_timeout() -> u64 {
    10800 // 3 hours
}

fn default_work_dir() -> PathBuf {
    PathBuf::from("/tmp/act-runner")
}

fn default_runner_file() -> PathBuf {
    PathBuf::from(".runner")
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config: {}", path.display()))?;
        serde_yaml::from_str(&content).context("failed to parse config")
    }

    pub fn api_base(&self) -> String {
        format!("{}/api/actions", self.instance.trim_end_matches('/'))
    }
}

/// Credentials persisted after registration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub uuid: String,
    pub token: String,
    pub name: String,
}

impl Credentials {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read runner file: {}", path.display()))?;
        serde_json::from_str(&content).context("failed to parse runner credentials")
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)
            .with_context(|| format!("failed to write runner file: {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn config_minimal_yaml() {
        let yaml = "instance: https://git.example.com\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.instance, "https://git.example.com");
        assert_eq!(cfg.capacity, 1);
        assert_eq!(cfg.fetch_interval, 2);
        assert_eq!(cfg.timeout, 10800);
        assert_eq!(cfg.labels, vec!["self-hosted:host"]);
        assert_eq!(cfg.work_dir, PathBuf::from("/tmp/act-runner"));
        assert_eq!(cfg.runner_file, PathBuf::from(".runner"));
    }

    #[test]
    fn config_full_yaml() {
        let yaml = r#"
instance: https://git.calii.net
name: my-runner
labels:
  - self-hosted:host
  - macos-arm64:host
capacity: 4
fetch_interval: 5
timeout: 7200
work_dir: /var/lib/runner
runner_file: /etc/runner/.runner
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.name, "my-runner");
        assert_eq!(cfg.capacity, 4);
        assert_eq!(cfg.fetch_interval, 5);
        assert_eq!(cfg.timeout, 7200);
        assert_eq!(cfg.labels.len(), 2);
        assert_eq!(cfg.work_dir, PathBuf::from("/var/lib/runner"));
    }

    #[test]
    fn api_base_strips_trailing_slash() {
        let yaml = "instance: https://git.example.com/\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.api_base(), "https://git.example.com/api/actions");
    }

    #[test]
    fn api_base_no_trailing_slash() {
        let yaml = "instance: https://git.example.com\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.api_base(), "https://git.example.com/api/actions");
    }

    #[test]
    fn credentials_roundtrip() {
        let dir = std::env::temp_dir().join("act-runner-test-creds");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".runner");

        let creds = Credentials {
            uuid: "abc-123".to_string(),
            token: "secret-token".to_string(),
            name: "test-runner".to_string(),
        };
        creds.save(&path).unwrap();

        let loaded = Credentials::load(&path).unwrap();
        assert_eq!(loaded.uuid, "abc-123");
        assert_eq!(loaded.token, "secret-token");
        assert_eq!(loaded.name, "test-runner");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn credentials_load_invalid_json() {
        let dir = std::env::temp_dir().join("act-runner-test-bad-creds");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".runner");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"not json").unwrap();

        assert!(Credentials::load(&path).is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn credentials_load_missing_file() {
        let path = PathBuf::from("/tmp/act-runner-test-nonexistent/.runner");
        assert!(Credentials::load(&path).is_err());
    }

    #[test]
    fn config_load_from_file() {
        let dir = std::env::temp_dir().join("act-runner-test-cfg");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(&path, "instance: https://example.com\ncapacity: 3\n").unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.instance, "https://example.com");
        assert_eq!(cfg.capacity, 3);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
