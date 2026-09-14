//! Principal resolution: who is calling, and what kind of caller are they.
//!
//! The rule this module exists to enforce: `actor_kind` and a human's identity come from the
//! credential, never the request body. A leaked agent token cannot pretend to be a human hold,
//! and a human token cannot claim work as someone else — exactly the scope-derivation discipline
//! the credential broker already uses. Requests may *ask*; only tokens may *say*.
//!
//! Two trust roots, both optional, at least one required by `serve`:
//! - **OIDC** (cloud): bearer JWTs verified against the issuer's JWKS (fpl-auth / SSO). Agents
//!   arrive as client-credentials tokens (a `client_id`), humans as user tokens (`sub`).
//! - **Static tokens** (local): files under `<state>/tokens/` whose *name* is the identity —
//!   `human-<name>` or `agent-<name>` — created by `marbles login` / `marbles agent-token`.
//!   This is how one machine runs the fleet without an IdP.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::types::ActorKind;

#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub kind: ActorKind,
    /// Stable identity: OIDC `sub`/`client_id`, or the token file's name suffix.
    pub name: String,
    /// Company/tenant claim when the issuer provides one; reserved for scoping.
    pub company_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthConfig {
    /// e.g. https://sso.fpl.dev — tokens must carry this `iss`.
    pub oidc_issuer: Option<String>,
    /// Expected `aud`.
    pub oidc_audience: Option<String>,
    /// Optional override; defaults to `<issuer>/.well-known/openid-configuration`.
    pub jwks_uri: Option<String>,
    /// `client_id`s that are agents; anything else authenticated is a human.
    pub agent_client_ids: Vec<String>,
    /// OIDC claim carrying the company/tenant, if the issuer emits one.
    pub company_claim: Option<String>,
}

pub struct Auth {
    config: AuthConfig,
    token_dir: PathBuf,
    jwks: Mutex<Option<(Instant, Jwks)>>,
}

#[derive(Clone, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

/// RSA/EC JWK subset; fields we do not consult remain for accurate (de)serialization.
#[derive(Clone, Deserialize)]
#[allow(dead_code)]
struct Jwk {
    kid: Option<String>,
    // Fields we accept from the wire and ignore; kept so unknown-key pain doesn't
    // hide behind serde's deny-by-default posture.
    #[serde(rename = "use", default)]
    use_: Option<String>,
    kty: String,
    // RSA components; jsonwebtoken consumes the map form.
    n: Option<String>,
    e: Option<String>,
    x: Option<String>,
    y: Option<String>,
    crv: Option<String>,
    alg: Option<String>,
}

#[derive(Deserialize)]
struct OidcDiscovery {
    jwks_uri: String,
}

impl Auth {
    pub fn new(config: AuthConfig, token_dir: PathBuf) -> Self {
        Self {
            config,
            token_dir,
            jwks: Mutex::new(None),
        }
    }

    /// Resolve a bearer token to a principal. The `Option` is present (even for the empty string)
    /// so the handler can distinguish "no header" from "unknown header" for consistent 401s.
    pub async fn authenticate(&self, bearer: Option<&str>) -> Option<Principal> {
        let token = bearer?.trim();
        if token.is_empty() {
            return None;
        }
        if let Some(principal) = self.static_token(token) {
            return Some(principal);
        }
        if self.config.oidc_issuer.is_some() {
            return self.oidc(token).await;
        }
        None
    }

    /// Local trust root: the token file's *name* is the identity; content is the secret.
    fn static_token(&self, token: &str) -> Option<Principal> {
        let entries = std::fs::read_dir(&self.token_dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            if content.trim() == token {
                if let Some(name) = stem.strip_prefix("human-") {
                    return Some(Principal {
                        kind: ActorKind::Human,
                        name: name.into(),
                        company_id: None,
                    });
                }
                if let Some(name) = stem.strip_prefix("agent-") {
                    return Some(Principal {
                        kind: ActorKind::Agent,
                        name: name.into(),
                        company_id: None,
                    });
                }
            }
        }
        None
    }

    async fn oidc(&self, token: &str) -> Option<Principal> {
        let issuer = self.config.oidc_issuer.as_deref()?;
        let header = jsonwebtoken::decode_header(token).ok()?;
        let kid = header.kid?;
        let jwks = self.jwks().await?;
        let key = jwks
            .keys
            .iter()
            .find(|k| k.kid.as_deref() == Some(kid.as_str()))?;
        let decoding = decoding_from_jwk(key)?;
        let algorithm: jsonwebtoken::Algorithm = key
            .alg
            .as_deref()
            .unwrap_or("RS256")
            .parse()
            .unwrap_or(jsonwebtoken::Algorithm::RS256);
        let mut validation = jsonwebtoken::Validation::new(algorithm);
        validation.set_issuer(&[issuer]);
        if let Some(aud) = &self.config.oidc_audience {
            validation.set_audience(&[aud]);
        }
        #[derive(Deserialize)]
        struct Claims {
            sub: String,
            #[serde(default)]
            client_id: Option<String>,
            #[serde(default)]
            azp: Option<String>,
        }
        let claims = jsonwebtoken::decode::<Claims>(token, &decoding, &validation)
            .ok()?
            .claims;
        let caller_client = claims.client_id.or(claims.azp);
        let is_agent = caller_client
            .as_ref()
            .is_some_and(|id| self.config.agent_client_ids.iter().any(|a| a == id));
        Some(Principal {
            kind: if is_agent {
                ActorKind::Agent
            } else {
                ActorKind::Human
            },
            name: caller_client.unwrap_or(claims.sub),
            company_id: self.config.company_claim.as_ref().and_then(|_| None),
        })
    }

    async fn jwks(&self) -> Option<Jwks> {
        const TTL: std::time::Duration = std::time::Duration::from_secs(900);
        if let Some((jwks, _)) = self
            .jwks
            .lock()
            .ok()?
            .as_ref()
            .filter(|(at, _)| at.elapsed() < TTL)
            .map(|(at, jwks)| (jwks.clone(), *at))
        {
            return Some(jwks);
        }
        let uri = match &self.config.jwks_uri {
            Some(uri) => uri.clone(),
            None => {
                let issuer = self.config.oidc_issuer.as_deref()?;
                let discovery: OidcDiscovery = http_get_json(&format!(
                    "{}/.well-known/openid-configuration",
                    issuer.trim_end_matches('/')
                ))
                .await
                .ok()?;
                discovery.jwks_uri
            }
        };
        let jwks: Jwks = http_get_json(&uri).await.ok()?;
        if let Ok(mut guard) = self.jwks.lock() {
            *guard = Some((Instant::now(), jwks.clone()));
        }
        Some(jwks)
    }
}

fn decoding_from_jwk(key: &Jwk) -> Option<jsonwebtoken::DecodingKey> {
    match key.kty.as_str() {
        "RSA" => Some(jsonwebtoken::DecodingKey::from_rsa_components(
            key.n.as_deref()?,
            key.e.as_deref()?,
        ))
        .transpose()
        .ok()
        .flatten(),
        "EC" => Some(jsonwebtoken::DecodingKey::from_ec_components(
            key.x.as_deref()?,
            key.y.as_deref()?,
        ))
        .transpose()
        .ok()
        .flatten(),
        _ => None,
    }
}

async fn http_get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(reqwest::Client::new);
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{url}: HTTP {status}"));
    }
    response.json().await.map_err(|e| e.to_string())
}

pub fn token_dir_default(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("tokens")
}

/// Mint a local token file and print the secret (files are 0600; the *name* is the identity).
pub fn mint_token(
    dir: &std::path::Path,
    kind: ActorKind,
    name: &str,
) -> std::io::Result<(PathBuf, String)> {
    std::fs::create_dir_all(dir)?;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| std::io::Error::other("no system randomness"))?;
    let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let prefix = match kind {
        ActorKind::Human => "human",
        ActorKind::Agent => "agent",
    };
    let path = dir.join(format!("{prefix}-{name}"));
    std::fs::write(&path, format!("{secret}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok((path, secret))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_token_names_are_the_identity() {
        let dir = std::env::temp_dir().join(format!("marbles-auth-{}", std::process::id()));
        let (path, secret) = mint_token(&dir, ActorKind::Agent, "codex-thread-1").unwrap();
        let auth = Auth::new(AuthConfig::default(), dir.clone());
        let principal = auth.static_token(&secret).expect("token resolves");
        assert_eq!(principal.kind, ActorKind::Agent);
        assert_eq!(principal.name, "codex-thread-1");
        assert!(auth.static_token("wrong-secret").is_none());
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn human_and_agent_files_cannot_be_confused() {
        let dir = std::env::temp_dir().join(format!("marbles-auth2-{}", std::process::id()));
        let (_, human) = mint_token(&dir, ActorKind::Human, "avery").unwrap();
        let (_, agent) = mint_token(&dir, ActorKind::Agent, "worker").unwrap();
        let auth = Auth::new(AuthConfig::default(), dir.clone());
        assert_eq!(auth.static_token(&human).unwrap().kind, ActorKind::Human);
        assert_eq!(auth.static_token(&agent).unwrap().kind, ActorKind::Agent);
        assert!(auth.static_token("nope").is_none());
        std::fs::remove_dir_all(dir).ok();
    }
}
