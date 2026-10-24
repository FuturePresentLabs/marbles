//! Local read cache for the HTTP client.
//!
//! When the tracker is unreachable, a read command (`list`, `ready`, `show`, `history`, `stats`,
//! `projects`) serves the newest successful response it has already seen and says so on stderr.
//! Writes are never cached: an outage must not look like a successful claim. This is deliberately
//! a cache, not replication — one server still owns the truth, clients only remember what it last
//! told them.
//!
//! Layout: `$MARBLES_HOME/cache/<sha256>.json`, one file per (server, op, request body), written
//! atomically via tmp+rename with mode 0600. The server URL, op, and request body are hashed into
//! the filename, so cache names leak neither endpoints nor any header/token material. Entries are
//! pruned to the newest few so an outage-heavy agent cannot grow the cache without bound.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config;

/// Bump when the on-disk entry shape changes; older files are then ignored (and pruned).
const ENTRY_VERSION: u32 = 1;
/// Newest entries kept per cache directory.
const MAX_ENTRIES: usize = 128;
/// Total byte budget for the cache directory.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    version: u32,
    server: String,
    op: String,
    /// Epoch seconds when the server produced this response.
    fetched_at: i64,
    /// The raw JSON body exactly as the server sent it, so replaying it parses identically.
    response: String,
}

/// A previously cached read response.
#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub body: String,
    pub fetched_at: i64,
}

/// Default location: `<state dir>/cache` (`$MARBLES_HOME/cache` or `~/.marbles/cache`).
pub fn default_root() -> PathBuf {
    config::state_dir().join("cache")
}

#[derive(Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, server: &str, op: &str, body: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(server.as_bytes());
        hasher.update([0u8]);
        hasher.update(op.as_bytes());
        hasher.update([0u8]);
        hasher.update(body.as_bytes());
        let digest = hasher.finalize();
        let mut name = String::with_capacity(64 + 5);
        for byte in digest {
            name.push(char::from(HEX[(byte >> 4) as usize]));
            name.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
        name.push_str(".json");
        self.root.join(name)
    }

    /// Record a successful read response. Best effort: callers ignore cache write failures.
    pub fn store(&self, server: &str, op: &str, body: &str, response: &str) -> Result<(), String> {
        fs::create_dir_all(&self.root)
            .map_err(|e| format!("creating {}: {e}", self.root.display()))?;
        restrict_dir(&self.root);
        let entry = Entry {
            version: ENTRY_VERSION,
            server: server.to_string(),
            op: op.to_string(),
            fetched_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or_default(),
            response: response.to_string(),
        };
        let encoded =
            serde_json::to_vec(&entry).map_err(|e| format!("encoding cache entry: {e}"))?;
        let path = self.path_for(server, op, body);
        // Write beside the target then rename: a crashed store never leaves a half entry behind.
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        write_private(&tmp, &encoded)?;
        fs::rename(&tmp, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("renaming {}: {e}", path.display())
        })?;
        self.prune();
        Ok(())
    }

    /// Newest cached response for this exact (server, op, body), if any.
    pub fn load(&self, server: &str, op: &str, body: &str) -> Option<CachedResponse> {
        let path = self.path_for(server, op, body);
        let text = fs::read_to_string(path).ok()?;
        let entry: Entry = serde_json::from_str(&text).ok()?;
        if entry.version != ENTRY_VERSION || entry.op != op || entry.server != server {
            return None;
        }
        Some(CachedResponse {
            body: entry.response,
            fetched_at: entry.fetched_at,
        })
    }

    /// Keep the newest `MAX_ENTRIES` files within the byte budget; drop the rest.
    fn prune(&self) {
        let Ok(dir) = fs::read_dir(&self.root) else {
            return;
        };
        let mut files: Vec<(PathBuf, SystemTime, u64)> = dir
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    return None;
                }
                let meta = entry.metadata().ok()?;
                let modified = meta.modified().unwrap_or(UNIX_EPOCH);
                Some((path, modified, meta.len()))
            })
            .collect();
        files.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        let mut bytes = 0u64;
        for (index, (path, _, len)) in files.into_iter().enumerate() {
            bytes = bytes.saturating_add(len);
            if index >= MAX_ENTRIES || bytes > MAX_BYTES {
                let _ = fs::remove_file(path);
            }
        }
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

#[cfg(unix)]
fn restrict_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) {}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("creating {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file =
        fs::File::create(path).map_err(|e| format!("creating {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

/// `2026-10-21T16:03Z`, the timestamp shown in the offline banner.
pub fn format_timestamp(epoch_seconds: i64) -> String {
    match chrono::DateTime::from_timestamp(epoch_seconds, 0) {
        Some(ts) => ts.format("%Y-%m-%dT%H:%MZ").to_string(),
        None => epoch_seconds.to_string(),
    }
}

/// Human age for the offline banner: `14m`, `3h`, `2d`.
pub fn humanize_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m", seconds / 60)
    } else if seconds < 60 * 60 * 24 {
        format!("{}h", seconds / (60 * 60))
    } else {
        format!("{}d", seconds / (60 * 60 * 24))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> (tempfile::TempDir, Cache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::at(dir.path().join("cache"));
        (dir, cache)
    }

    #[test]
    fn round_trips_keyed_by_server_op_and_body() {
        let (_dir, cache) = cache();
        cache
            .store(
                "https://marbles.fpl.dev",
                "issues.ready",
                "{}",
                "[{\"id\":\"a\"}]",
            )
            .unwrap();
        let hit = cache
            .load("https://marbles.fpl.dev", "issues.ready", "{}")
            .unwrap();
        assert_eq!(hit.body, "[{\"id\":\"a\"}]");
        // Different body/server/op is a different key: never cross-served.
        assert!(
            cache
                .load(
                    "https://marbles.fpl.dev",
                    "issues.ready",
                    "{\"project\":\"x\"}"
                )
                .is_none()
        );
        assert!(
            cache
                .load("https://other.example", "issues.ready", "{}")
                .is_none()
        );
        assert!(
            cache
                .load("https://marbles.fpl.dev", "stats", "{}")
                .is_none()
        );
    }

    #[test]
    fn filenames_and_contents_carry_no_token_or_plaintext_keys() {
        let (dir, cache) = cache();
        let secret = "super-secret-bearer-token";
        cache
            .store(
                "https://marbles.fpl.dev",
                "issues.list",
                "{\"project\":\"fleet\"}",
                "[]",
            )
            .unwrap();
        let mut found = 0;
        for entry in fs::read_dir(dir.path().join("cache")).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(!name.contains(secret));
            assert!(!name.contains("issues.list"));
            assert!(!name.contains("marbles.fpl.dev"));
            assert_eq!(name.len(), 64 + ".json".len());
            let contents = fs::read_to_string(entry.path()).unwrap();
            assert!(!contents.contains(secret));
            found += 1;
        }
        assert_eq!(found, 1);
    }

    #[test]
    fn prunes_to_the_newest_entries() {
        let (_dir, cache) = cache();
        for index in 0..(MAX_ENTRIES + 5) {
            cache
                .store(
                    "https://marbles.fpl.dev",
                    "issues.get",
                    &format!("{{\"id\":\"{index}\"}}"),
                    "[]",
                )
                .unwrap();
            // Distinct mtimes so "newest" is well defined.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let count = fs::read_dir(cache.root()).unwrap().count();
        assert_eq!(count, MAX_ENTRIES);
        // The newest key survived; the oldest did not.
        assert!(
            cache
                .load(
                    "https://marbles.fpl.dev",
                    "issues.get",
                    &format!("{{\"id\":\"{}\"}}", MAX_ENTRIES + 4)
                )
                .is_some()
        );
        assert!(
            cache
                .load("https://marbles.fpl.dev", "issues.get", "{\"id\":\"0\"}")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, cache) = cache();
        cache
            .store("https://marbles.fpl.dev", "stats", "{}", "{}")
            .unwrap();
        let file = fs::read_dir(dir.path().join("cache"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let mode = file.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(dir.path().join("cache"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn timestamps_and_ages_read_like_a_human_wrote_them() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00Z");
        assert_eq!(humanize_age(0), "0s");
        assert_eq!(humanize_age(59), "59s");
        assert_eq!(humanize_age(14 * 60), "14m");
        assert_eq!(humanize_age(3 * 60 * 60), "3h");
        assert_eq!(humanize_age(2 * 24 * 60 * 60), "2d");
    }
}
