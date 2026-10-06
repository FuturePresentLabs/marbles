//! OIDC authorization-code login and refresh for the hosted Marbles service.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config;

const DEFAULT_ISSUER: &str = "https://sso.fpl.dev";
const DEFAULT_CLIENT_ID: &str = "marbles";
const DEFAULT_REDIRECT_URI: &str = "http://localhost:17878/callback";
/// OIDC token calls are bounded like tracker calls; a hung IdP must not hang the CLI.
pub const DEFAULT_OIDC_TIMEOUT_MS: u64 = 8_000;
const CONNECT_TIMEOUT_MS: u64 = 3_000;

#[derive(Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub url: String,
    #[serde(default = "default_issuer")]
    pub issuer: String,
    #[serde(default = "default_client_id")]
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_redirect_uri")]
    pub redirect_uri: String,
    pub refresh_token: String,
    pub id_token: String,
    pub expires_at: i64,
    #[serde(skip, default = "persist_by_default")]
    persist: bool,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    /// Some providers only return a rotated refresh token on rotation, not every refresh.
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default = "default_expires_in")]
    expires_in: i64,
}

impl TokenResponse {
    fn expiration(&self) -> Result<i64, String> {
        let id_token = self
            .id_token
            .as_deref()
            .ok_or_else(|| "issuer returned no id_token".to_owned())?;
        if id_token.trim().is_empty() || self.expires_in <= 0 {
            return Err("issuer returned an empty token or invalid lifetime".into());
        }
        chrono::Utc::now()
            .timestamp()
            .checked_add(self.expires_in)
            .ok_or_else(|| "issuer returned an invalid token lifetime".to_owned())
    }
}

#[derive(Debug, Deserialize)]
struct OAuthError {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

fn default_expires_in() -> i64 {
    300
}

fn default_issuer() -> String {
    DEFAULT_ISSUER.to_owned()
}
fn default_client_id() -> String {
    DEFAULT_CLIENT_ID.to_owned()
}
fn default_redirect_uri() -> String {
    DEFAULT_REDIRECT_URI.to_owned()
}

fn persist_by_default() -> bool {
    true
}

fn http_client(timeout_ms: u64) -> Result<reqwest::Client, String> {
    let timeout = std::time::Duration::from_millis(timeout_ms.max(1));
    reqwest::Client::builder()
        .connect_timeout(timeout.min(std::time::Duration::from_millis(CONNECT_TIMEOUT_MS)))
        .timeout(timeout)
        // Credentials must never follow an HTTP redirect to an unintended host.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("building OIDC HTTP client: {e}"))
}

fn env_config() -> Result<Option<ClientConfig>, String> {
    let refresh_token = match std::env::var("MARBLES_OIDC_REFRESH_TOKEN") {
        Ok(value) if !value.trim().is_empty() => value,
        Ok(_) | Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(err) => return Err(format!("reading MARBLES_OIDC_REFRESH_TOKEN: {err}")),
    };
    let required = |name: &str| {
        std::env::var(name)
            .map_err(|err| format!("{name} is required with MARBLES_OIDC_REFRESH_TOKEN: {err}"))
            .and_then(|value| {
                if value.trim().is_empty() {
                    Err(format!("{name} must not be empty"))
                } else {
                    Ok(value)
                }
            })
    };
    Ok(Some(ClientConfig {
        url: std::env::var("MARBLES_URL").unwrap_or_else(|_| "https://marbles.fpl.dev".to_owned()),
        issuer: std::env::var("MARBLES_OIDC_ISSUER").unwrap_or_else(|_| default_issuer()),
        client_id: required("MARBLES_OIDC_CLIENT_ID")?,
        client_secret: required("MARBLES_OIDC_CLIENT_SECRET")?,
        redirect_uri: default_redirect_uri(),
        refresh_token,
        id_token: String::new(),
        expires_at: 0,
        persist: false,
    }))
}

pub fn client_config_path() -> PathBuf {
    config::state_dir().join("client.toml")
}

pub fn load() -> Result<Option<ClientConfig>, String> {
    if let Some(config) = env_config()? {
        return Ok(Some(config));
    }
    let path = client_config_path();
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map(Some).map_err(|_| {
            format!(
                "invalid credential configuration in {}; sign in again",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    }
}

fn save(value: &ClientConfig) -> Result<(), String> {
    save_at(value, &client_config_path())
}

fn private_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn save_at(value: &ClientConfig, path: &std::path::Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "invalid client config path".to_owned())?;
    fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    let temp = path.with_extension(format!("toml.{}.tmp", random_urlsafe(12)?));
    let result = (|| {
        let mut file = private_options()
            .create_new(true)
            .open(&temp)
            .map_err(|e| format!("creating credential file: {e}"))?;
        file.write_all(
            toml::to_string_pretty(value)
                .map_err(|e| e.to_string())?
                .as_bytes(),
        )
        .map_err(|e| format!("writing credentials: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("syncing credentials: {e}"))?;
        fs::rename(&temp, path).map_err(|e| format!("installing credentials: {e}"))?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("syncing credential directory: {e}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn token_error(operation: &str, status: reqwest::StatusCode, body: &str) -> String {
    // Never echo provider bodies: they can contain submitted credentials or tokens.
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        });
    let hint = match code.as_deref() {
        Some("invalid_grant") => "session expired or revoked; sign in again",
        Some("invalid_client" | "unauthorized_client") => {
            "client registration is invalid; check the configured client"
        }
        _ => "provider rejected the request; retry or check the issuer",
    };
    format!("OIDC {operation} HTTP {}: {hint}", status.as_u16())
}

fn random_urlsafe(bytes: usize) -> Result<String, String> {
    let mut value = vec![0_u8; bytes];
    getrandom::fill(&mut value).map_err(|e| format!("generating OAuth nonce: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}

pub async fn login(
    url: &str,
    issuer: Option<&str>,
    client_id: Option<&str>,
    client_secret: &str,
    redirect_uri: Option<&str>,
) -> Result<ClientConfig, String> {
    let issuer = issuer.unwrap_or(DEFAULT_ISSUER).trim_end_matches('/');
    let client_id = client_id.unwrap_or(DEFAULT_CLIENT_ID);
    let redirect_uri = redirect_uri.unwrap_or(DEFAULT_REDIRECT_URI);
    let parsed_redirect = reqwest::Url::parse(redirect_uri).map_err(|e| e.to_string())?;
    if parsed_redirect.host_str() != Some("localhost") {
        return Err("OIDC callback must use localhost".to_owned());
    }
    let port = parsed_redirect
        .port_or_known_default()
        .ok_or("callback URI has no port")?;
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("listening for OIDC callback on {port}: {e}"))?;
    let state = random_urlsafe(24)?;
    let verifier = random_urlsafe(48)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut authorize =
        reqwest::Url::parse(&format!("{issuer}/authorize")).map_err(|e| e.to_string())?;
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", "openid email profile offline_access")
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    open_browser(authorize.as_str())?;
    eprintln!("Complete sign-in in your browser; waiting on {redirect_uri} …");
    let callback = tokio::task::spawn_blocking(move || receive_callback(listener))
        .await
        .map_err(|e| format!("OIDC callback task failed: {e}"))??;
    if callback.state.as_deref() != Some(&state) {
        return Err("OIDC callback state mismatch".to_owned());
    }
    let code = callback.code.ok_or_else(|| {
        callback
            .error
            .unwrap_or_else(|| "OIDC callback omitted code".to_owned())
    })?;
    let response = http_client(DEFAULT_OIDC_TIMEOUT_MS)?
        .post(format!("{issuer}/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("exchanging OIDC code: {e}"))?;
    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(token_error("token exchange", status, &body));
    }
    let tokens: TokenResponse =
        serde_json::from_str(&body).map_err(|_| "invalid OIDC token response".to_owned())?;
    let expires_at = tokens.expiration()?;
    let refresh_token = tokens
        .refresh_token
        .filter(|token| !token.is_empty())
        .ok_or("issuer did not grant offline access; sign in with offline_access")?;
    let id_token = tokens
        .id_token
        .ok_or_else(|| "OIDC login response omitted id_token".to_string())?;
    let cfg = ClientConfig {
        url: url.trim_end_matches('/').to_owned(),
        issuer: issuer.to_owned(),
        client_id: client_id.to_owned(),
        client_secret: client_secret.to_owned(),
        redirect_uri: redirect_uri.to_owned(),
        refresh_token,
        id_token,
        expires_at,
        persist: true,
    };
    if cfg.persist {
        let _lock = credential_lock(client_config_path()).await?;
        save(&cfg)?;
    }
    Ok(cfg)
}

pub async fn valid_token(cfg: ClientConfig) -> Result<(ClientConfig, String), String> {
    valid_token_with_timeout(cfg, DEFAULT_OIDC_TIMEOUT_MS).await
}

pub async fn valid_token_with_timeout(
    cfg: ClientConfig,
    timeout_ms: u64,
) -> Result<(ClientConfig, String), String> {
    valid_token_at_with_timeout(cfg, client_config_path(), timeout_ms).await
}

#[cfg_attr(not(test), allow(dead_code))]
async fn valid_token_at(
    cfg: ClientConfig,
    path: PathBuf,
) -> Result<(ClientConfig, String), String> {
    valid_token_at_with_timeout(cfg, path, DEFAULT_OIDC_TIMEOUT_MS).await
}

async fn valid_token_at_with_timeout(
    mut cfg: ClientConfig,
    path: PathBuf,
    timeout_ms: u64,
) -> Result<(ClientConfig, String), String> {
    let _lock = if cfg.persist {
        Some(credential_lock(path.clone()).await?)
    } else {
        None
    };
    if cfg.persist {
        match fs::read_to_string(&path) {
            Ok(text) => {
                let latest: ClientConfig = toml::from_str(&text)
                    .map_err(|_| "invalid saved credentials; sign in again".to_owned())?;
                if latest.url != cfg.url
                    || latest.issuer != cfg.issuer
                    || latest.client_id != cfg.client_id
                {
                    return Err(
                        "saved Marbles profile changed during refresh; retry the command"
                            .to_owned(),
                    );
                }
                cfg = latest;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("reading saved credentials: {e}")),
        }
    }
    if !cfg.id_token.is_empty() && cfg.expires_at > chrono::Utc::now().timestamp() + 60 {
        return Ok((cfg.clone(), cfg.id_token.clone()));
    }
    let response = http_client(timeout_ms)?
        .post(format!("{}/token", cfg.issuer.trim_end_matches('/')))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("refresh_token", cfg.refresh_token.as_str()),
        ])
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                format!(
                    "OIDC refresh timed out after {timeout_ms}ms; the tracker is not usable until the IdP answers or you re-login"
                )
            } else {
                format!("refreshing OIDC token: {e}")
            }
        })?;
    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(refresh_error(status.as_u16(), &body));
    }
    let tokens: TokenResponse =
        serde_json::from_str(&body).map_err(|_| "invalid OIDC refresh response".to_owned())?;
    apply_refresh(&mut cfg, tokens)?;
    if cfg.persist {
        save_at(&cfg, &path)?;
    }
    Ok((cfg.clone(), cfg.id_token))
}

/// Classify a failed refresh. `invalid_grant` is the one case that is definitively terminal:
/// the refresh token is expired/revoked and only an interactive login can fix it.
fn refresh_error(status: u16, body: &str) -> String {
    let parsed = serde_json::from_str::<OAuthError>(body).ok();
    let oauth_error = parsed.as_ref().and_then(|e| e.error.as_deref());
    let description = parsed
        .as_ref()
        .and_then(|e| e.error_description.as_deref())
        .unwrap_or("");
    if status == 400 && oauth_error == Some("invalid_grant") {
        let detail = if description.is_empty() {
            String::new()
        } else {
            format!(": {description}")
        };
        return format!(
            "re-login required: OIDC refresh was rejected (invalid_grant{detail}); run `marbles login --client-secret <secret>` to mint new tokens"
        );
    }
    // Everything else is reported by kind and status only; provider bodies may carry secrets.
    let status =
        reqwest::StatusCode::from_u16(status).unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    token_error("refresh", status, body)
}

/// Apply a refresh response. A missing rotated refresh token keeps the current one; a missing
/// id_token is terminal because Marbles authenticates with the id_token, not an access token.
fn apply_refresh(cfg: &mut ClientConfig, tokens: TokenResponse) -> Result<(), String> {
    let id_token = tokens.id_token.ok_or_else(|| {
        "re-login required: OIDC refresh response omitted id_token; run `marbles login --client-secret <secret>`"
            .to_string()
    })?;
    if id_token.trim().is_empty() {
        return Err(
            "re-login required: OIDC refresh response omitted id_token; run `marbles login --client-secret <secret>`"
                .to_string(),
        );
    }
    let expires_at = chrono::Utc::now()
        .timestamp()
        .checked_add(tokens.expires_in)
        .filter(|_| tokens.expires_in > 0)
        .ok_or_else(|| {
            "OIDC refresh response had an invalid lifetime; retry the command or sign in again"
                .to_string()
        })?;
    if let Some(refresh_token) = tokens.refresh_token.filter(|token| !token.is_empty()) {
        cfg.refresh_token = refresh_token;
    }
    cfg.id_token = id_token;
    cfg.expires_at = expires_at;
    Ok(())
}

async fn credential_lock(path: PathBuf) -> Result<fs::File, String> {
    // Lock the sidecar, not client.toml (which is atomically replaced). Kernel locks
    // release on process exit; no stale lock cleanup or secret-bearing lock file.
    let lock_path = path.with_extension("refresh.lock");
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let file = private_options()
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|e| format!("opening refresh lock: {e}"))?;
        let began = Instant::now();
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(file),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if began.elapsed() > Duration::from_secs(15) {
                        return Err(
                            "another Marbles process is refreshing; retry shortly".to_owned()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(format!("locking credentials: {e}")),
            }
        }
    })
    .await
    .map_err(|e| format!("refresh lock task: {e}"))?
}

struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

fn receive_callback(listener: TcpListener) -> Result<Callback, String> {
    let (mut stream, _) = listener
        .accept()
        .map_err(|e| format!("accepting OIDC callback: {e}"))?;
    let mut bytes = [0_u8; 8192];
    let count = stream
        .read(&mut bytes)
        .map_err(|e| format!("reading OIDC callback: {e}"))?;
    let request = String::from_utf8_lossy(&bytes[..count]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or("malformed OIDC callback")?;
    let url =
        reqwest::Url::parse(&format!("http://localhost{target}")).map_err(|e| e.to_string())?;
    let find = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    let result = Callback {
        code: find("code"),
        state: find("state"),
        error: find("error"),
    };
    let body = "Marbles sign-in complete. You can close this tab.";
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(reply.as_bytes())
        .map_err(|e| format!("replying to OIDC callback: {e}"))?;
    Ok(result)
}

fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let status = Command::new("open").arg(url).status();
    #[cfg(target_os = "linux")]
    let status = Command::new("xdg-open").arg(url).status();
    #[cfg(target_os = "windows")]
    let status = Command::new("cmd").args(["/C", "start", "", url]).status();
    status
        .map_err(|e| format!("opening browser: {e}"))
        .and_then(|s| {
            if s.success() {
                Ok(())
            } else {
                Err("browser opener failed".to_owned())
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ClientConfig {
        ClientConfig {
            url: "http://127.0.0.1:1".into(),
            issuer: "https://sso.example".into(),
            client_id: "marbles".into(),
            client_secret: "secret".into(),
            redirect_uri: default_redirect_uri(),
            refresh_token: "old-refresh".into(),
            id_token: "old-id".into(),
            expires_at: 0,
            persist: false,
        }
    }

    #[test]
    fn defaults_are_stable() {
        assert_eq!(default_client_id(), "marbles");
        assert_eq!(default_redirect_uri(), "http://localhost:17878/callback");
        assert_eq!(DEFAULT_OIDC_TIMEOUT_MS, 8_000);
    }

    #[test]
    fn invalid_grant_is_a_terminal_relogin_error() {
        let message = refresh_error(
            400,
            r#"{"error":"invalid_grant","error_description":"Token is not active"}"#,
        );
        assert!(message.contains("re-login required"), "{message}");
        assert!(message.contains("marbles login"), "{message}");
        assert!(message.contains("Token is not active"), "{message}");
    }

    #[test]
    fn a_server_or_network_failure_is_not_misreported_as_relogin() {
        let message = refresh_error(503, "upstream unavailable");
        assert!(!message.contains("re-login required"), "{message}");
        assert!(message.contains("503"), "{message}");
    }

    #[test]
    fn refresh_keeps_the_old_token_when_provider_omits_rotation() {
        let mut cfg = config();
        apply_refresh(
            &mut cfg,
            TokenResponse {
                refresh_token: None,
                id_token: Some("new-id".into()),
                expires_in: 60,
            },
        )
        .unwrap();
        assert_eq!(cfg.refresh_token, "old-refresh");
        assert_eq!(cfg.id_token, "new-id");
    }

    #[test]
    fn refresh_without_an_id_token_requires_login() {
        let mut cfg = config();
        let err = apply_refresh(
            &mut cfg,
            TokenResponse {
                refresh_token: Some("rotated".into()),
                id_token: None,
                expires_in: 60,
            },
        )
        .unwrap_err();
        assert!(err.contains("re-login required"), "{err}");
        assert_eq!(
            cfg.id_token, "old-id",
            "nothing is mutated before the check"
        );
    }

    #[tokio::test]
    async fn a_fresh_token_is_returned_without_an_idp_round_trip() {
        let mut cfg = config();
        cfg.expires_at = chrono::Utc::now().timestamp() + 3_600;
        cfg.id_token = "still-fresh".into();
        let (out, token) = valid_token_with_timeout(cfg, 1).await.unwrap();
        assert_eq!(token, "still-fresh");
        assert_eq!(out.id_token, "still-fresh");
    }

    #[test]
    fn invalid_token_lifetimes_cannot_replace_a_working_profile() {
        for (id_token, expires_in) in [("", 3600), ("id", 0), ("id", -1), ("id", i64::MAX)] {
            assert!(
                TokenResponse {
                    refresh_token: None,
                    id_token: Some(id_token.into()),
                    expires_in
                }
                .expiration()
                .is_err()
            );
        }
    }

    fn expired(issuer: String, persist: bool) -> ClientConfig {
        ClientConfig {
            url: "https://marbles.example".into(),
            issuer,
            client_id: "marbles".into(),
            client_secret: "private-client-secret".into(),
            redirect_uri: default_redirect_uri(),
            refresh_token: "initial-refresh".into(),
            id_token: String::new(),
            expires_at: 0,
            persist,
        }
    }

    async fn issuer(
        rotates: bool,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(move || {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    let mut response =
                        serde_json::json!({"id_token":"fresh-id", "expires_in":3600});
                    if rotates {
                        response["refresh_token"] = "rotated-refresh".into();
                    }
                    axum::Json(response)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, count, task)
    }

    #[tokio::test]
    async fn concurrent_expired_profiles_refresh_only_once_and_save_rotation() {
        use std::sync::atomic::Ordering;
        let (url, count, server) = issuer(true).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.toml");
        let cfg = expired(url, true);
        save_at(&cfg, &path).unwrap();
        let (first, second) = tokio::join!(
            valid_token_at(cfg.clone(), path.clone()),
            valid_token_at(cfg, path.clone())
        );
        assert_eq!(first.unwrap().1, "fresh-id");
        assert_eq!(second.unwrap().1, "fresh-id");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let saved: ClientConfig = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.refresh_token, "rotated-refresh");
        // A subsequent process uses the saved token without contacting the issuer.
        valid_token_at(saved, path.clone()).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn environment_refresh_never_overwrites_saved_profile() {
        let (url, _, server) = issuer(true).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.toml");
        fs::write(&path, "unchanged browser profile").unwrap();
        let (cfg, token) = valid_token_at(expired(url, false), path.clone())
            .await
            .unwrap();
        assert_eq!(token, "fresh-id");
        assert_eq!(cfg.refresh_token, "rotated-refresh");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "unchanged browser profile"
        );
        assert!(!path.with_extension("refresh.lock").exists());
        server.abort();
    }

    #[tokio::test]
    async fn refresh_without_rotation_retains_refresh_token() {
        let (url, _, server) = issuer(false).await;
        let dir = tempfile::tempdir().unwrap();
        let (cfg, _) = valid_token_at(expired(url, false), dir.path().join("client.toml"))
            .await
            .unwrap();
        assert_eq!(cfg.refresh_token, "initial-refresh");
        server.abort();
    }

    #[test]
    fn provider_errors_never_echo_secrets() {
        let error = token_error(
            "refresh",
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_grant","error_description":"private-refresh-token"}"#,
        );
        assert!(error.contains("sign in again"));
        assert!(!error.contains("private-refresh-token"));
        let error = token_error(
            "refresh",
            reqwest::StatusCode::BAD_REQUEST,
            "private-refresh-token",
        );
        assert!(!error.contains("private-refresh-token"));
    }

    #[tokio::test]
    async fn profile_switch_fails_without_overwriting_new_login() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.toml");
        let stale = expired("https://old.example".into(), true);
        let new = expired("https://new.example".into(), true);
        save_at(&new, &path).unwrap();
        assert!(
            valid_token_at(stale, path.clone())
                .await
                .err()
                .unwrap()
                .contains("profile changed")
        );
        let saved: ClientConfig = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved.issuer, "https://new.example");
    }
}
