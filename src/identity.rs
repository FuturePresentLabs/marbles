//! Per-store Ed25519 identity and the public `marbles.store/1` descriptor.
//!
//! A store is not just a database file: federation needs to know *which* store answered and that
//! the answer was signed by the key it expects. Each store therefore owns a stable `store_id`, one
//! or more named Ed25519 keys, and a signed descriptor that can be verified offline from a pinned
//! `(store_id, key_id, public_key)` triple.
//!
//! Key rotation is an explicit overlap operation rather than a config edit: the old key keeps
//! verifying until its `retired_at` instant, and fails closed afterwards. Private keys live only in
//! the identity file (written with mode 0600); they are never part of the descriptor and never
//! rendered by `Debug`.
//!
//! The federation path deliberately does not reuse the webhook HMAC secret: that secret is a shared
//! delivery credential, not a store identity, and a shared secret cannot answer "who signed this?".

use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use openssl::pkey::{Id, PKey, Private};
use openssl::sign::{Signer, Verifier};
use serde::{Deserialize, Serialize};

pub const DESCRIPTOR_SCHEMA: &str = "marbles.store/1";
pub const IDENTITY_FILE_VERSION: u32 = 1;
pub const DEFAULT_OVERLAP_SECONDS: i64 = 24 * 60 * 60;
pub const DEFAULT_CAPABILITIES: [&str; 3] = ["issues", "claims", "plans"];

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("no store identity at {path}; run `mb store identity init`")]
    NotFound { path: PathBuf },
    #[error("reading store identity {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writing store identity {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("store identity file is not valid TOML: {0}")]
    Parse(String),
    #[error("store identity has no active key {0:?}")]
    UnknownKey(String),
    #[error("store identity has no active key")]
    NoActiveKey,
    #[error("store identity is empty (missing store_id)")]
    EmptyStoreId,
    #[error("descriptor schema is {found:?}, expected {DESCRIPTOR_SCHEMA:?}")]
    WrongSchema { found: String },
    #[error("descriptor store_id {found:?} does not match pinned store_id {expected:?}")]
    StoreMismatch { expected: String, found: String },
    #[error("descriptor key_id {found:?} does not match pinned key_id {expected:?}")]
    KeyMismatch { expected: String, found: String },
    #[error("descriptor public key does not match the pinned public key for key {0:?}")]
    PublicKeyMismatch(String),
    #[error("key {0:?} retired at {1}; no longer trusted")]
    KeyRetired(String, i64),
    #[error("descriptor is unsigned")]
    UnsignedDescriptor,
    #[error("descriptor signature is malformed: {0}")]
    MalformedSignature(String),
    #[error("descriptor signature does not verify for key {0:?}")]
    BadSignature(String),
    #[error("crypto failure: {0}")]
    Crypto(#[from] openssl::error::ErrorStack),
    #[error("random source failed: {0}")]
    Random(String),
    #[error("base64 failure: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("JSON failure: {0}")]
    Json(#[from] serde_json::Error),
}

/// One named key belonging to a store. The private half is persisted but never exposed.
#[derive(Clone, Serialize, Deserialize)]
pub struct IdentityKey {
    /// Human-meaningful, stable name, e.g. `store_ab12/key-1`.
    pub key_id: String,
    /// base64url (unpadded) raw 32-byte Ed25519 public key.
    pub public_key: String,
    /// base64 (standard) PKCS#8 DER private key. Secret; never printed.
    private_key: String,
    pub created_at: i64,
    /// Exclusive instant after which this key no longer verifies (rotation overlap end).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<i64>,
}

impl fmt::Debug for IdentityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentityKey")
            .field("key_id", &self.key_id)
            .field("public_key", &self.public_key)
            .field("private_key", &"<redacted>")
            .field("created_at", &self.created_at)
            .field("retired_at", &self.retired_at)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotationRecord {
    pub old_key_id: String,
    pub new_key_id: String,
    pub rotated_at: i64,
    pub overlap_until: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StoreIdentity {
    pub version: u32,
    pub store_id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub issued_at: i64,
    pub active_key_id: String,
    pub keys: Vec<IdentityKey>,
    #[serde(default)]
    pub rotations: Vec<RotationRecord>,
}

impl fmt::Debug for StoreIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreIdentity")
            .field("version", &self.version)
            .field("store_id", &self.store_id)
            .field("display_name", &self.display_name)
            .field("endpoint", &self.endpoint)
            .field("capabilities", &self.capabilities)
            .field("issued_at", &self.issued_at)
            .field("active_key_id", &self.active_key_id)
            .field("keys", &self.keys)
            .field("rotations", &self.rotations)
            .finish()
    }
}

/// The public, signed `marbles.store/1` descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreDescriptor {
    pub schema: String,
    pub store_id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub key_id: String,
    pub public_key: String,
    pub capabilities: Vec<String>,
    pub issued_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl StoreDescriptor {
    /// Bytes the signature covers: the descriptor without its signature field, serialized in the
    /// struct's declared field order.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, IdentityError> {
        let mut copy = self.clone();
        copy.signature = None;
        Ok(serde_json::to_vec(&copy)?)
    }

    pub fn to_json_pretty(&self) -> Result<String, IdentityError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn from_json(json: &str) -> Result<Self, IdentityError> {
        Ok(serde_json::from_str(json)?)
    }
}

/// What an offline verifier pins: exactly which store and key it trusts, and for how long.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedKey {
    pub store_id: String,
    pub key_id: String,
    pub public_key: String,
    /// Exclusive instant after which this key is no longer trusted (rotation overlap end).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<i64>,
}

/// Options for creating a brand-new store identity.
#[derive(Debug, Clone)]
pub struct InitOptions {
    pub display_name: String,
    pub endpoint: Option<String>,
    pub capabilities: Vec<String>,
}

impl InitOptions {
    pub fn new(display_name: impl Into<String>) -> Self {
        Self {
            display_name: display_name.into(),
            endpoint: None,
            capabilities: DEFAULT_CAPABILITIES
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
        }
    }
}

impl StoreIdentity {
    /// Load an existing identity, or create one at `path` if it does not exist yet.
    ///
    /// Idempotent by construction: an existing identity is returned untouched, so `init` twice
    /// never rotates or renames anything.
    pub fn load_or_init(path: &Path, opts: &InitOptions, now: i64) -> Result<Self, IdentityError> {
        match Self::load(path) {
            Ok(identity) => Ok(identity),
            Err(IdentityError::NotFound { .. }) => {
                let identity = Self::create(opts, now)?;
                identity.save(path)?;
                Ok(identity)
            }
            Err(err) => Err(err),
        }
    }

    pub fn load(path: &Path) -> Result<Self, IdentityError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(IdentityError::NotFound {
                    path: path.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(IdentityError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let identity: Self =
            toml::from_str(&text).map_err(|err| IdentityError::Parse(err.to_string()))?;
        if identity.store_id.trim().is_empty() {
            return Err(IdentityError::EmptyStoreId);
        }
        Ok(identity)
    }

    /// Persist the identity, including private keys, with owner-only permissions.
    pub fn save(&self, path: &Path) -> Result<(), IdentityError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| IdentityError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text =
            toml::to_string_pretty(self).map_err(|err| IdentityError::Parse(err.to_string()))?;
        std::fs::write(path, text).map_err(|source| IdentityError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        restrict_permissions(path)?;
        Ok(())
    }

    /// Create a fresh identity with a new store id and first key.
    pub fn create(opts: &InitOptions, now: i64) -> Result<Self, IdentityError> {
        let store_id = new_store_id()?;
        let key = IdentityKey::generate(&store_id, 1, now)?;
        Ok(Self {
            version: IDENTITY_FILE_VERSION,
            store_id,
            display_name: opts.display_name.clone(),
            endpoint: opts.endpoint.clone(),
            capabilities: opts.capabilities.clone(),
            issued_at: now,
            active_key_id: key.key_id.clone(),
            keys: vec![key],
            rotations: Vec::new(),
        })
    }

    /// Create and save an identity at `path`.
    pub fn init_at(path: &Path, opts: &InitOptions, now: i64) -> Result<Self, IdentityError> {
        Self::load_or_init(path, opts, now)
    }

    pub fn active_key(&self) -> Result<&IdentityKey, IdentityError> {
        self.keys
            .iter()
            .find(|key| key.key_id == self.active_key_id)
            .ok_or_else(|| IdentityError::UnknownKey(self.active_key_id.clone()))
    }

    /// Build and sign a descriptor with the currently active key.
    pub fn descriptor(&self, now: i64) -> Result<StoreDescriptor, IdentityError> {
        let key = self.active_key()?;
        let mut descriptor = StoreDescriptor {
            schema: DESCRIPTOR_SCHEMA.to_string(),
            store_id: self.store_id.clone(),
            display_name: self.display_name.clone(),
            endpoint: self.endpoint.clone(),
            key_id: key.key_id.clone(),
            public_key: key.public_key.clone(),
            capabilities: self.capabilities.clone(),
            issued_at: now,
            signature: None,
        };
        let signing_key = key.private_key()?;
        let mut signer = Signer::new_without_digest(&signing_key)?;
        let signature = signer.sign_oneshot_to_vec(&descriptor.signing_bytes()?)?;
        descriptor.signature = Some(URL_SAFE_NO_PAD.encode(signature));
        Ok(descriptor)
    }

    /// Descriptor JSON for this store, signed at `now`.
    pub fn descriptor_json(&self, now: i64) -> Result<String, IdentityError> {
        self.descriptor(now)?.to_json_pretty()
    }

    /// The pin an offline verifier should record for `key_id`.
    pub fn pin(&self, key_id: &str) -> Result<PinnedKey, IdentityError> {
        let key = self
            .keys
            .iter()
            .find(|key| key.key_id == key_id)
            .ok_or_else(|| IdentityError::UnknownKey(key_id.to_string()))?;
        Ok(PinnedKey {
            store_id: self.store_id.clone(),
            key_id: key.key_id.clone(),
            public_key: key.public_key.clone(),
            valid_until: key.retired_at,
        })
    }

    /// Pin the currently active key.
    pub fn active_pin(&self) -> Result<PinnedKey, IdentityError> {
        self.pin(&self.active_key_id)
    }

    /// Verify a descriptor against this identity's own trusted keys at instant `now`.
    ///
    /// This is the server-side/CLI path; offline verifiers use [`verify_descriptor`] with a
    /// [`PinnedKey`] instead and never need the identity file.
    pub fn verify_descriptor(
        &self,
        descriptor_json: &str,
        now: i64,
    ) -> Result<StoreDescriptor, IdentityError> {
        let descriptor = StoreDescriptor::from_json(descriptor_json)?;
        let pin = self.pin(&descriptor.key_id)?;
        verify_descriptor(&descriptor, &pin, now)
    }

    /// Rotate to a newly named key, retiring the old one after `overlap_seconds`.
    ///
    /// During `[rotated_at, overlap_until)` both keys verify. At or after `overlap_until` the old
    /// key is rejected. The new key becomes the signing key immediately.
    pub fn rotate(
        &mut self,
        now: i64,
        overlap_seconds: i64,
    ) -> Result<RotationRecord, IdentityError> {
        let old_key = self.active_key()?.clone();
        let overlap_seconds = overlap_seconds.max(0);
        let overlap_until = now.saturating_add(overlap_seconds);
        let new_key = IdentityKey::generate(&self.store_id, self.keys.len() + 1, now)?;
        let record = RotationRecord {
            old_key_id: old_key.key_id.clone(),
            new_key_id: new_key.key_id.clone(),
            rotated_at: now,
            overlap_until,
        };
        for key in &mut self.keys {
            if key.key_id == old_key.key_id {
                // A second rotation before the first overlap ends keeps the earliest retirement
                // instant: an old key is never resurrected by a later rotation.
                key.retired_at = Some(match key.retired_at {
                    Some(existing) => existing.min(overlap_until),
                    None => overlap_until,
                });
            }
        }
        self.issued_at = now;
        self.active_key_id = new_key.key_id.clone();
        self.keys.push(new_key);
        self.rotations.push(record.clone());
        Ok(record)
    }

    /// Keys trusted at `now` (active and any key still inside its overlap window).
    pub fn trusted_keys_at(&self, now: i64) -> Vec<&IdentityKey> {
        self.keys
            .iter()
            .filter(|key| key.retired_at.is_none_or(|end| now < end))
            .collect()
    }

    /// Public, log-safe view of the identity: no private key material.
    pub fn public_view(&self) -> serde_json::Value {
        serde_json::json!({
            "version": self.version,
            "store_id": self.store_id,
            "display_name": self.display_name,
            "endpoint": self.endpoint,
            "capabilities": self.capabilities,
            "issued_at": self.issued_at,
            "active_key_id": self.active_key_id,
            "keys": self.keys.iter().map(|key| serde_json::json!({
                "key_id": key.key_id,
                "public_key": key.public_key,
                "created_at": key.created_at,
                "retired_at": key.retired_at,
            })).collect::<Vec<_>>(),
            "rotations": self.rotations,
        })
    }
}

impl IdentityKey {
    fn generate(store_id: &str, ordinal: usize, now: i64) -> Result<Self, IdentityError> {
        let key = PKey::generate_ed25519()?;
        let public_key = key.raw_public_key()?;
        let private = key.private_key_to_der()?;
        Ok(Self {
            key_id: format!("{store_id}/key-{ordinal}"),
            public_key: URL_SAFE_NO_PAD.encode(public_key),
            private_key: STANDARD.encode(private),
            created_at: now,
            retired_at: None,
        })
    }

    fn private_key(&self) -> Result<PKey<Private>, IdentityError> {
        let der = STANDARD.decode(&self.private_key)?;
        Ok(PKey::private_key_from_der(&der)?)
    }
}

/// Verify a parsed descriptor offline from a pinned key at instant `now`.
///
/// Fails closed: any mismatch (schema, store, key id, public key, retirement, or signature) is an
/// error, and a descriptor without a signature is never accepted.
pub fn verify_descriptor(
    descriptor: &StoreDescriptor,
    pin: &PinnedKey,
    now: i64,
) -> Result<StoreDescriptor, IdentityError> {
    if descriptor.schema != DESCRIPTOR_SCHEMA {
        return Err(IdentityError::WrongSchema {
            found: descriptor.schema.clone(),
        });
    }
    if pin.store_id.is_empty() || descriptor.store_id != pin.store_id {
        return Err(IdentityError::StoreMismatch {
            expected: pin.store_id.clone(),
            found: descriptor.store_id.clone(),
        });
    }
    if descriptor.key_id != pin.key_id {
        return Err(IdentityError::KeyMismatch {
            expected: pin.key_id.clone(),
            found: descriptor.key_id.clone(),
        });
    }
    if descriptor.public_key != pin.public_key {
        return Err(IdentityError::PublicKeyMismatch(pin.key_id.clone()));
    }
    if let Some(valid_until) = pin.valid_until
        && now >= valid_until
    {
        return Err(IdentityError::KeyRetired(pin.key_id.clone(), valid_until));
    }
    let signature = descriptor
        .signature
        .as_deref()
        .ok_or(IdentityError::UnsignedDescriptor)?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|err| IdentityError::MalformedSignature(err.to_string()))?;
    let public = URL_SAFE_NO_PAD
        .decode(&descriptor.public_key)
        .map_err(|err| IdentityError::MalformedSignature(err.to_string()))?;
    let key = PKey::public_key_from_raw_bytes(&public, Id::ED25519)?;
    let mut verifier = Verifier::new_without_digest(&key)?;
    let valid = verifier
        .verify_oneshot(&signature, &descriptor.signing_bytes()?)
        .unwrap_or(false);
    if !valid {
        return Err(IdentityError::BadSignature(descriptor.key_id.clone()));
    }
    Ok(descriptor.clone())
}

/// Verify descriptor JSON directly, for callers that only hold the wire bytes.
pub fn verify_descriptor_json(
    descriptor_json: &str,
    pin: &PinnedKey,
    now: i64,
) -> Result<StoreDescriptor, IdentityError> {
    verify_descriptor(&StoreDescriptor::from_json(descriptor_json)?, pin, now)
}

fn new_store_id() -> Result<String, IdentityError> {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).map_err(|err| IdentityError::Random(err.to_string()))?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("store_{hex}"))
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), IdentityError> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, permissions).map_err(|source| IdentityError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), IdentityError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_identity(dir: &tempfile::TempDir, name: &str) -> StoreIdentity {
        let path = dir.path().join(name).join("store-identity.toml");
        StoreIdentity::init_at(&path, &InitOptions::new(name), 1_000).unwrap()
    }

    #[test]
    fn identities_are_stable_across_reload_and_distinct_between_stores() {
        let dir = tempfile::tempdir().unwrap();
        let one = temp_identity(&dir, "one");
        let path = dir.path().join("one/store-identity.toml");
        let reloaded = StoreIdentity::load(&path).unwrap();
        assert_eq!(one.store_id, reloaded.store_id);
        assert_eq!(one.active_key_id, reloaded.active_key_id);

        let again = StoreIdentity::init_at(&path, &InitOptions::new("changed"), 9_999).unwrap();
        assert_eq!(again.store_id, one.store_id);
        assert_eq!(again.display_name, "one");

        let two = temp_identity(&dir, "two");
        assert_ne!(one.store_id, two.store_id);
        assert_ne!(one.active_key_id, two.active_key_id);
    }

    #[test]
    fn descriptor_debug_and_json_never_contain_private_material() {
        let dir = tempfile::tempdir().unwrap();
        let identity = temp_identity(&dir, "one");
        let secret = identity.active_key().unwrap().private_key.clone();
        let descriptor_json = identity.descriptor_json(1_100).unwrap();
        assert!(!descriptor_json.contains(&secret));
        let debug = format!("{identity:?}");
        assert!(!debug.contains(&secret));
        assert!(debug.contains("<redacted>"));
    }
}
