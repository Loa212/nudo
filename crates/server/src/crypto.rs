//! Secret encryption, password hashing, token generation and webhook signature
//! verification.
//!
//! Secrets are sealed with AES-256-GCM under a single key supplied by the
//! operator. Each value gets a fresh random 96-bit nonce which is stored
//! alongside the ciphertext, so the same plaintext never encrypts to the same
//! bytes twice.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use rand::TryRngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// AES-256-GCM nonce width in bytes.
const NONCE_LEN: usize = 12;

/// The master key for the secret store.
#[derive(Clone)]
pub struct SecretKey([u8; 32]);

impl SecretKey {
    /// Parses a key from hex (64 chars) or base64. Both are accepted because
    /// operators paste whichever their tooling produced.
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("secret key is empty");
        }

        let bytes = if raw.len() == 64 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
            hex::decode(raw).context("decoding hex secret key")?
        } else {
            B64.decode(raw)
                .context("secret key is neither 64 hex chars nor valid base64")?
        };

        let bytes: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
            anyhow!("secret key must be 32 bytes, got {}", v.len())
        })?;
        Ok(Self(bytes))
    }

    /// Generates a fresh random key.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng
            .try_fill_bytes(&mut bytes)
            .expect("OS entropy");
        Self(bytes)
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Seals a plaintext. The returned blob is `nonce || ciphertext`, base64
    /// encoded so it can live in a SQLite `TEXT` column.
    pub fn seal(&self, plaintext: &str) -> anyhow::Result<String> {
        let cipher = Aes256Gcm::new_from_slice(&self.0).expect("32-byte key");

        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rngs::OsRng
            .try_fill_bytes(&mut nonce_bytes)
            .expect("OS entropy");
        let nonce = Nonce::try_from(&nonce_bytes[..]).expect("12-byte nonce");

        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| anyhow!("sealing secret failed"))?;

        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        Ok(B64.encode(blob))
    }

    /// Opens a blob produced by [`SecretKey::seal`]. Fails if the key is wrong
    /// or the ciphertext was tampered with — GCM authenticates, so a modified
    /// blob is an error rather than garbage plaintext.
    pub fn open(&self, blob: &str) -> anyhow::Result<String> {
        let blob = B64.decode(blob.trim()).context("decoding sealed secret")?;
        if blob.len() <= NONCE_LEN {
            bail!("sealed secret is truncated");
        }

        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new_from_slice(&self.0).expect("32-byte key");
        let plaintext = cipher
            .decrypt(
                &Nonce::try_from(nonce_bytes).expect("12-byte nonce"),
                ciphertext,
            )
            .map_err(|_| anyhow!("opening secret failed: wrong key or corrupt ciphertext"))?;

        String::from_utf8(plaintext).context("secret is not valid UTF-8")
    }
}

impl std::fmt::Debug for SecretKey {
    /// Never print the key material, not even in a panic backtrace.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(redacted)")
    }
}

/// Hex sha256 of a value. Used as the secret store's drift digest and to key
/// the GitHub setup-state cache by hash rather than by the raw state.
pub fn sha256_hex(value: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(value.as_ref()))
}

/// A random URL-safe token, used for API tokens, session ids, terminal session
/// tokens and GitHub `state` values.
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng
        .try_fill_bytes(&mut buf)
        .expect("OS entropy");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Hashes a password with argon2id using per-password random salt.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng as ArgonRng};

    let salt = SaltString::generate(&mut ArgonRng);
    let hash = argon2::Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow!("hashing password: {e}"))?;
    Ok(hash.to_string())
}

/// Verifies a password against a stored argon2 hash. A malformed stored hash
/// verifies as `false` rather than erroring, so a corrupt row cannot be used to
/// distinguish "no such user" from "bad hash".
pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};

    match PasswordHash::new(hash) {
        Ok(parsed) => argon2::Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Verifies a GitHub webhook signature.
///
/// `header` is the raw `X-Hub-Signature-256` value, which GitHub sends as
/// `sha256=<hex>`; `body` must be the **raw** request bytes, before any JSON
/// parsing, because re-serializing changes the bytes the HMAC covers. The
/// comparison is constant-time.
pub fn verify_github_signature(header: &str, body: &[u8], secret: &str) -> bool {
    use hmac::{Hmac, Mac};

    // A missing prefix is a malformed header, not something to be lenient
    // about: accepting a bare hex digest would let a caller who can only
    // control part of the header still match.
    let Some(provided_hex) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(provided) = hex::decode(provided_hex) else {
        return false;
    };

    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    let expected = mac.finalize().into_bytes();

    // Length is checked first because ct_eq on different lengths is not
    // meaningful; the length itself is not secret.
    provided.len() == expected.len() && bool::from(provided.ct_eq(&expected))
}

/// Constant-time equality for secrets compared as strings — API tokens,
/// terminal session tokens, CSRF tokens.
pub fn secure_compare(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_secrets_open_back_to_the_original_plaintext() {
        let key = SecretKey::generate();
        let secret = "postgres://user:pw@db/app?sslmode=require";
        let sealed = key.seal(secret).expect("seal");
        assert_eq!(key.open(&sealed).expect("open"), secret);
    }

    #[test]
    fn sealing_the_same_value_twice_produces_different_ciphertext() {
        // A deterministic nonce would leak which secrets are equal.
        let key = SecretKey::generate();
        let a = key.seal("same").expect("seal");
        let b = key.seal("same").expect("seal");
        assert_ne!(a, b);
        assert_eq!(key.open(&a).expect("open"), key.open(&b).expect("open"));
    }

    #[test]
    fn a_secret_cannot_be_opened_with_a_different_key() {
        let sealed = SecretKey::generate().seal("value").expect("seal");
        assert!(SecretKey::generate().open(&sealed).is_err());
    }

    #[test]
    fn tampering_with_the_ciphertext_is_detected() {
        let key = SecretKey::generate();
        let sealed = key.seal("value").expect("seal");

        let mut raw = B64.decode(&sealed).expect("decode");
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        assert!(key.open(&B64.encode(raw)).is_err());
    }

    #[test]
    fn truncated_blobs_are_rejected_rather_than_panicking() {
        let key = SecretKey::generate();
        assert!(key.open(&B64.encode([0u8; 8])).is_err());
        assert!(key.open("").is_err());
        assert!(key.open("not base64!!!").is_err());
    }

    #[test]
    fn keys_parse_from_both_hex_and_base64() {
        let key = SecretKey::generate();
        let hex_form = key.to_hex();
        let b64_form = B64.encode(key.0);

        assert_eq!(SecretKey::parse(&hex_form).expect("hex").0, key.0);
        assert_eq!(SecretKey::parse(&b64_form).expect("b64").0, key.0);
    }

    #[test]
    fn keys_of_the_wrong_length_are_rejected() {
        assert!(SecretKey::parse(&hex::encode([0u8; 16])).is_err());
        assert!(SecretKey::parse("").is_err());
    }

    #[test]
    fn passwords_verify_only_against_their_own_hash() {
        let hash = hash_password("correct horse battery staple").expect("hash");
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("Correct horse battery staple", &hash));
        assert!(!verify_password("", &hash));
    }

    #[test]
    fn a_corrupt_stored_hash_verifies_as_false() {
        assert!(!verify_password("anything", "not-a-phc-string"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn the_same_password_hashes_differently_each_time() {
        let a = hash_password("pw").expect("hash");
        let b = hash_password("pw").expect("hash");
        assert_ne!(a, b, "salt must be per-password");
        assert!(verify_password("pw", &a) && verify_password("pw", &b));
    }

    // GitHub's documented example vector, so the implementation is checked
    // against the real service rather than only against itself.
    const GH_SECRET: &str = "It's a Secret to Everybody";
    const GH_BODY: &[u8] = b"Hello, World!";
    const GH_SIGNATURE: &str =
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    #[test]
    fn the_documented_github_signature_vector_verifies() {
        assert!(verify_github_signature(GH_SIGNATURE, GH_BODY, GH_SECRET));
    }

    #[test]
    fn a_signature_over_different_bytes_is_rejected() {
        assert!(!verify_github_signature(GH_SIGNATURE, b"Hello, World", GH_SECRET));
        assert!(!verify_github_signature(GH_SIGNATURE, b"", GH_SECRET));
    }

    #[test]
    fn a_signature_under_the_wrong_secret_is_rejected() {
        assert!(!verify_github_signature(GH_SIGNATURE, GH_BODY, "wrong secret"));
    }

    #[test]
    fn signatures_without_the_sha256_prefix_are_rejected() {
        // The bare digest is correct for this body, but the header shape is
        // wrong and we do not accept it.
        let bare = GH_SIGNATURE.trim_start_matches("sha256=");
        assert!(!verify_github_signature(bare, GH_BODY, GH_SECRET));
        assert!(!verify_github_signature(&format!("sha1={bare}"), GH_BODY, GH_SECRET));
        assert!(!verify_github_signature("", GH_BODY, GH_SECRET));
    }

    #[test]
    fn malformed_and_wrong_length_digests_are_rejected() {
        assert!(!verify_github_signature("sha256=nothex", GH_BODY, GH_SECRET));
        assert!(!verify_github_signature("sha256=aabb", GH_BODY, GH_SECRET));
        // A correct digest with a trailing byte appended must not pass.
        assert!(!verify_github_signature(
            &format!("{GH_SIGNATURE}00"),
            GH_BODY,
            GH_SECRET
        ));
    }

    #[test]
    fn secure_compare_matches_only_identical_strings() {
        assert!(secure_compare("token", "token"));
        assert!(!secure_compare("token", "tokeN"));
        assert!(!secure_compare("token", "token "));
        assert!(secure_compare("", ""));
    }

    #[test]
    fn digests_are_stable_and_distinguish_values() {
        // The digest is the secret store's drift signal, so it must be
        // deterministic across processes.
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(sha256_hex("a"), sha256_hex("b"));
    }

    #[test]
    fn random_tokens_are_unique_and_url_safe() {
        let a = random_token(32);
        let b = random_token(32);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
