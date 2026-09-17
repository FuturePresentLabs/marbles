//! OIDC authorization-code login and refresh for the hosted Marbles service.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config;

const DEFAULT_ISSUER: &str = "https://sso.fpl.dev";
const DEFAULT_CLIENT_ID: &str = "marbles";
const DEFAULT_REDIRECT_URI: &str = "http://localhost:17878/callback";

#[derive(Clone, Debug, Serialize, Deserialize)]
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
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    refresh_token: String,
    id_token: String,
    expires_in: i64,
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

pub fn client_config_path() -> PathBuf {
    config::state_dir().join("client.toml")
}

pub fn load() -> Result<Option<ClientConfig>, String> {
    let path = client_config_path();
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text)
            .map(Some)
            .map_err(|e| format!("parsing {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    }
}

fn save(value: &ClientConfig) -> Result<(), String> {
    let path = client_config_path();
    let parent = path
        .parent()
        .ok_or_else(|| "invalid client config path".to_owned())?;
    fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    let temp = path.with_extension("toml.tmp");
    fs::write(
        &temp,
        toml::to_string_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("writing {}: {e}", temp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("securing {}: {e}", temp.display()))?;
    }
    fs::rename(&temp, &path).map_err(|e| format!("installing {}: {e}", path.display()))
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
    let response = reqwest::Client::new()
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
        return Err(format!(
            "OIDC token exchange HTTP {}: {}",
            status.as_u16(),
            body.trim()
        ));
    }
    let tokens: TokenResponse =
        serde_json::from_str(&body).map_err(|e| format!("parsing OIDC token response: {e}"))?;
    let cfg = ClientConfig {
        url: url.trim_end_matches('/').to_owned(),
        issuer: issuer.to_owned(),
        client_id: client_id.to_owned(),
        client_secret: client_secret.to_owned(),
        redirect_uri: redirect_uri.to_owned(),
        refresh_token: tokens.refresh_token,
        id_token: tokens.id_token,
        expires_at: chrono::Utc::now().timestamp() + tokens.expires_in,
    };
    save(&cfg)?;
    Ok(cfg)
}

pub async fn valid_token(mut cfg: ClientConfig) -> Result<(ClientConfig, String), String> {
    if cfg.expires_at > chrono::Utc::now().timestamp() + 60 {
        return Ok((cfg.clone(), cfg.id_token.clone()));
    }
    let response = reqwest::Client::new()
        .post(format!("{}/token", cfg.issuer.trim_end_matches('/')))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("refresh_token", cfg.refresh_token.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("refreshing OIDC token: {e}"))?;
    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "OIDC refresh HTTP {}: {}",
            status.as_u16(),
            body.trim()
        ));
    }
    let tokens: TokenResponse =
        serde_json::from_str(&body).map_err(|e| format!("parsing OIDC refresh: {e}"))?;
    cfg.refresh_token = tokens.refresh_token;
    cfg.id_token = tokens.id_token;
    cfg.expires_at = chrono::Utc::now().timestamp() + tokens.expires_in;
    save(&cfg)?;
    Ok((cfg.clone(), cfg.id_token))
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
    #[test]
    fn defaults_are_stable() {
        assert_eq!(default_client_id(), "marbles");
        assert_eq!(default_redirect_uri(), "http://localhost:17878/callback");
    }
}
