use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use ipnet::IpNet;
use serde::Deserialize;

pub const CONFIG_ENV: &str = "MEDIA_SEARCH_CONFIG";
pub const LLM_URL_ENV: &str = "MEDIA_SEARCH_LLM_URL";
pub const LLM_KEY_ENV: &str = "MEDIA_SEARCH_LLM_API_KEY";

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub staging: StagingConfig,
    #[serde(default)]
    pub roots: Vec<RootConfig>,
    pub llm: LlmConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub allowed_cidrs: Vec<IpNet>,
    #[serde(default = "default_max_body")]
    pub max_body_bytes: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StagingConfig {
    pub dir: PathBuf,
    pub max_bytes: u64,
    pub max_file_bytes: u64,
    #[serde(default = "default_index_workers")]
    pub index_workers: usize,
    #[serde(default = "default_true")]
    pub copy_on_index: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RootConfig {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub timeout_secs: u64,
    #[serde(default = "default_one")]
    pub workers: usize,
    pub image_max_edge: u32,
    pub video_frame_interval_secs: u32,
    pub video_max_frames: u32,
}

fn default_max_body() -> usize {
    64 * 1024
}
fn default_index_workers() -> usize {
    4
}
fn default_one() -> usize {
    1
}
fn default_true() -> bool {
    true
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let path = std::env::var(CONFIG_ENV).unwrap_or_else(|_| "config.toml".to_string());
        Self::from_file(Path::new(&path))
    }

    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let mut cfg = Self::from_toml(&raw)?;
        if let Ok(url) = std::env::var(LLM_URL_ENV) {
            cfg.llm.base_url = url;
        }
        if let Ok(key) = std::env::var(LLM_KEY_ENV) {
            cfg.llm.api_key = Some(key);
        }
        Ok(cfg)
    }

    pub fn from_toml(raw: &str) -> anyhow::Result<Self> {
        let cfg: Config = toml::from_str(raw).context("parsing config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.server.allowed_cidrs.is_empty() {
            bail!("server.allowed_cidrs must not be empty");
        }
        let mut names = std::collections::HashSet::new();
        for root in &self.roots {
            if root.name.is_empty() || root.name.contains('/') {
                bail!("invalid root name {:?}", root.name);
            }
            if !names.insert(&root.name) {
                bail!("duplicate root name {:?}", root.name);
            }
            if !root.path.is_absolute() {
                bail!("root {:?} must have an absolute path", root.name);
            }
        }
        if self.staging.index_workers == 0 || self.llm.workers == 0 {
            bail!("worker counts must be >= 1");
        }
        if self.llm.video_frame_interval_secs == 0 {
            bail!("llm.video_frame_interval_secs must be >= 1");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let raw = include_str!("../config.example.toml");
        let cfg = Config::from_toml(raw).unwrap();
        assert_eq!(cfg.roots.len(), 1);
        assert!(cfg.staging.copy_on_index);
    }

    #[test]
    fn rejects_relative_root() {
        let raw =
            include_str!("../config.example.toml").replace("/mnt/share/example", "relative/path");
        assert!(Config::from_toml(&raw).is_err());
    }
}
