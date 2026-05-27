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
    #[serde(default = "default_db")]
    pub db_path: String,
    /// Directory the generated site is written into (the GitHub Pages root).
    #[serde(default = "default_output_dir")]
    pub output_dir: String,
    /// Custom domain; when set, a `CNAME` file is written into `output_dir`.
    #[serde(default)]
    pub custom_domain: Option<String>,
    /// Absolute base URL of the published site, e.g. `https://news.example.com`.
    /// Used by the `digest` subcommand to build deep-links. When unset, it is
    /// derived from `custom_domain`.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Model passed to `codex exec -m`. CLI `--model` overrides this.
    #[serde(default = "default_model")]
    pub model: String,
    /// Reasoning effort passed to codex (`model_reasoning_effort`): e.g.
    /// `minimal`, `low`, `medium`, `high`. CLI `--thinking` overrides this.
    #[serde(default = "default_thinking")]
    pub thinking: String,
}

impl Settings {
    /// Absolute base URL (no trailing slash) for building deep-links. Prefers an
    /// explicit `base_url`, else derives `https://{custom_domain}`. Returns an
    /// error when neither is configured.
    pub fn site_base_url(&self) -> Result<String> {
        if let Some(b) = &self.base_url {
            let trimmed = b.trim_end_matches('/');
            if trimmed.is_empty() {
                anyhow::bail!("base_url in [settings] must not be empty");
            }
            return Ok(trimmed.to_string());
        }
        if let Some(d) = &self.custom_domain {
            let d = d.trim_end_matches('/');
            if d.is_empty() {
                anyhow::bail!("custom_domain in [settings] must not be empty");
            }
            if d.contains("://") {
                anyhow::bail!("custom_domain must be a bare hostname, not a URL; set base_url for a full URL");
            }
            return Ok(format!("https://{d}"));
        }
        anyhow::bail!("no base_url or custom_domain set in [settings]; one is required to build deep-links")
    }
}

fn default_db() -> String {
    "~/.news-fetcher/news.db".into()
}
fn default_output_dir() -> String {
    "docs".into()
}
fn default_model() -> String {
    "gpt-5.3-codex".into()
}
fn default_thinking() -> String {
    "medium".into()
}

/// Expand a leading `~` or `~/` to the user's home directory (`$HOME`). Paths
/// without a `~` prefix, or when `$HOME` is unset, are returned unchanged.
pub fn expand_tilde(path: &str) -> String {
    expand_tilde_with(path, home_dir().as_deref())
}

/// Core of [`expand_tilde`], parameterised on the home dir so it's testable
/// without mutating process environment.
fn expand_tilde_with(path: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return path.to_string();
    };
    if path == "~" {
        return home.to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("{}/{}", home.trim_end_matches('/'), rest);
    }
    path.to_string()
}

fn home_dir() -> Option<String> {
    std::env::var("HOME").ok().filter(|h| !h.is_empty())
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
        let mut cfg: Config = toml::from_str(&raw).context("parsing config.toml")?;
        cfg.settings.db_path = expand_tilde(&cfg.settings.db_path);
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(base_url: Option<&str>, custom_domain: Option<&str>) -> Settings {
        Settings {
            keywords: vec![],
            db_path: default_db(),
            output_dir: default_output_dir(),
            custom_domain: custom_domain.map(String::from),
            base_url: base_url.map(String::from),
            model: default_model(),
            thinking: default_thinking(),
        }
    }

    #[test]
    fn base_url_prefers_explicit_setting() {
        let s = settings(Some("https://explicit.example/"), Some("derived.example"));
        assert_eq!(s.site_base_url().unwrap(), "https://explicit.example");
    }

    #[test]
    fn base_url_derives_from_custom_domain() {
        let s = settings(None, Some("ainews.dob.cc"));
        assert_eq!(s.site_base_url().unwrap(), "https://ainews.dob.cc");
    }

    #[test]
    fn base_url_errors_when_neither_set() {
        let s = settings(None, None);
        assert!(s.site_base_url().is_err());
    }

    #[test]
    fn base_url_rejects_empty_string() {
        assert!(settings(Some(""), None).site_base_url().is_err());
        assert!(settings(Some("/"), None).site_base_url().is_err());
    }

    #[test]
    fn custom_domain_rejects_empty_string() {
        assert!(settings(None, Some("")).site_base_url().is_err());
    }

    #[test]
    fn custom_domain_rejects_url_with_scheme() {
        assert!(settings(None, Some("https://ainews.dob.cc")).site_base_url().is_err());
    }

    #[test]
    fn expand_tilde_expands_home_prefix() {
        let home = Some("/home/alice");
        assert_eq!(expand_tilde_with("~/.news-fetcher/news.db", home), "/home/alice/.news-fetcher/news.db");
        assert_eq!(expand_tilde_with("~", home), "/home/alice");
    }

    #[test]
    fn expand_tilde_leaves_other_paths_untouched() {
        let home = Some("/home/alice");
        assert_eq!(expand_tilde_with("news.db", home), "news.db");
        assert_eq!(expand_tilde_with("/abs/news.db", home), "/abs/news.db");
        // A bare `~user` form is not expanded (only `~` and `~/`).
        assert_eq!(expand_tilde_with("~bob/x", home), "~bob/x");
    }

    #[test]
    fn expand_tilde_without_home_is_noop() {
        assert_eq!(expand_tilde_with("~/x", None), "~/x");
    }
}
