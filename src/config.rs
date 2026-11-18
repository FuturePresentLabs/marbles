//! Config and state locations. Everything marbles owns on a machine lives under one directory
//! (`$MARBLES_HOME`, default `~/.marbles`): the database, tokens, and local client state. A
//! repo's `.marbles/project.toml` is the only thing that lives inside a checkout — a pointer, not a
//! store.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

/// Ed25519 store identity + `marbles.store/1` descriptor metadata. Contains private keys, so the
/// file is written with owner-only permissions and must never be copied into a repo or artifact.
pub fn identity_path() -> PathBuf {
    state_dir().join("store-identity.toml")
}

/// Local pins for remote stores (`name`, locator, verified `(store_id, key_id, public_key)`).
/// Client state only: it never authorizes a write and never travels over the tracker API.
pub fn remotes_path() -> PathBuf {
    state_dir().join("remotes.json")
}

/// Local cache of verified remote issue assertions, one entry per `(store_id, remote_issue_id)`.
/// Client state only: nothing here is uploaded and nothing here can mutate a remote store.
pub fn assertion_cache_dir() -> PathBuf {
    state_dir().join("assertions")
}

/// Per-project file: `<root>/.marbles/project.toml` with `slug` and `prefix`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectFile {
    pub slug: String,
    pub prefix: String,
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
            "slug = \"demo\"\nprefix = \"demo\"\n",
        )
        .unwrap();
        let (root, project) = discover_project(&deep).unwrap();
        assert_eq!(project.slug, "demo");
        assert!(root.ends_with("a"));
    }
}
