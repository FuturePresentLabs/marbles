//! Config and state locations. Everything marbles owns on a machine lives under one directory
//! (`$MARBLES_HOME`, default `~/.marbles`): the database, tokens, and the server config. A repo's
//! `.marbles/project.toml` is the only thing that lives inside a checkout — a pointer, not a store.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::auth::AuthConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    pub url: String,
    pub secret_file: PathBuf,
    #[serde(default = "default_webhook_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_webhook_timeout_seconds() -> u64 {
    5
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            company_store_root: None,
            webhook: None,
            auth: AuthConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Hosted mode: route every authenticated request to
    /// `<company_store_root>/<verified-company-id>/marbles.db`.
    /// When absent, retain the single local database used by workstation mode.
    #[serde(default)]
    pub company_store_root: Option<PathBuf>,
    /// Optional signed event sink. Delivery is backed by each store's transactional outbox.
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
    #[serde(default)]
    pub auth: AuthConfig,
}

fn default_listen() -> String {
    "127.0.0.1:7878".to_string()
}

pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MARBLES_HOME") {
        return dir.into();
    }
    dirs::home_dir()
        .map(|h| h.join(".marbles"))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn db_path() -> PathBuf {
    state_dir().join("marbles.db")
}

pub fn token_dir() -> PathBuf {
    state_dir().join("tokens")
}

pub fn server_config_path() -> PathBuf {
    state_dir().join("server.toml")
}

pub fn server_config() -> ServerConfig {
    match std::fs::read_to_string(server_config_path()) {
        Ok(text) => toml::from_str(&text).expect("server.toml must parse if present"),
        Err(_) => ServerConfig::default(),
    }
}

/// Per-project file: `<root>/.marbles/project.toml` with `slug` and `prefix`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectFile {
    pub slug: String,
    pub prefix: String,
    #[serde(default)]
    pub server_url: Option<String>,
}

pub const PROJECT_FILE: &str = ".marbles/project.toml";

/// Walk up from `dir` looking for `.marbles/project.toml`, like git finds `.git`.
pub fn discover_project(dir: &Path) -> Option<(PathBuf, ProjectFile)> {
    let mut cursor = dir.canonicalize().ok()?;
    loop {
        let candidate = cursor.join(PROJECT_FILE);
        if candidate.exists() {
            let text = std::fs::read_to_string(&candidate).ok()?;
            return Some((cursor, toml::from_str(&text).ok()?));
        }
        if !cursor.pop() {
            return None;
        }
    }
}

pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.extend(ch.to_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_lowercase_single_dash() {
        assert_eq!(slugify("Legion of BOM"), "legion-of-bom");
        assert_eq!(slugify("__dolt_remote__"), "dolt-remote");
    }

    #[test]
    fn discovery_walks_up_to_the_project_file() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        let project_root = dir.path().join("a");
        std::fs::create_dir_all(project_root.join(".marbles")).unwrap();
        std::fs::write(
            project_root.join(".marbles/project.toml"),
            "slug = \"demo\"\nprefix = \"demo\"\nserver_url = \"https://marbles.fpl.dev\"\n",
        )
        .unwrap();
        let (root, project) = discover_project(&deep).unwrap();
        assert_eq!(project.slug, "demo");
        assert_eq!(
            project.server_url.as_deref(),
            Some("https://marbles.fpl.dev")
        );
        assert!(root.ends_with("a"));
    }
}
