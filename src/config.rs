use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub settings: Settings,
    #[serde(default, rename = "source")]
    pub sources: Vec<Source>,
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default = "default_max_items")]
    pub max_items: usize,
    #[serde(default = "default_db")]
    pub db_path: String,
    #[serde(default = "default_html")]
    pub output_html: String,
}

fn default_max_items() -> usize {
    100
}
fn default_db() -> String {
    "news.db".into()
}
fn default_html() -> String {
    "index.html".into()
}

/// One configured source. Fields are optional and validated per `kind`.
#[derive(Debug, Deserialize)]
pub struct Source {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub always_relevant: bool,
    // kind-specific:
    pub repo: Option<String>,
    pub query: Option<String>,
    pub subreddit: Option<String>,
    pub url: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw).context("parsing sources.toml")?;
        Ok(cfg)
    }
}
