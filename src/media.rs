//! Stateless media-token signer for the inbound `/media` proxy.
//!
//! GOWA auto-downloads inbound media to `statics/media/<file>` on its loopback. The agent is a
//! separate box and can't reach GOWA directly, so the shim re-serves the file via
//! `GET /media/<token>` and forwards *that* URL to the agent (see [`crate::agent`]). The token is a
//! stateless HMAC over the GOWA-relative path, so the proxy needs no server state — it survives a
//! restart with nothing to reload — and the token can be neither forged nor used for path traversal.
//!
//! Token format: `<base64url(path)>.<hex(HMAC_SHA256(key, path))>`. [`verify`] recomputes the MAC
//! with a constant-time compare (mirroring [`crate::gowa::verify_signature`]), then rejects any path
//! that doesn't live under `statics/media/` or that contains a `..` traversal component. The signing
//! key reuses the per-tenant `GOWA_WEBHOOK_SECRET` — no new secret to provision.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// The only directory the media proxy will serve from. A verified path must start with this and
/// must not contain a `..`, so a token can never address a file outside GOWA's media store.
const MEDIA_PREFIX: &str = "statics/media/";

/// Sign a GOWA-relative media path into an opaque, URL-safe token. The path is base64url-encoded
/// (so `/` and `.` survive a URL path segment) and tagged with an HMAC over the *raw* path bytes.
pub fn sign(key: &[u8], gowa_path: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(gowa_path.as_bytes());
    let tag = hex::encode(mac.finalize().into_bytes());
    let encoded = URL_SAFE_NO_PAD.encode(gowa_path.as_bytes());
    format!("{encoded}.{tag}")
}

/// Verify a token and return the GOWA-relative path it authorises, or `None` if the token is
/// malformed, its MAC doesn't match the key, or the decoded path escapes `statics/media/`. The MAC
/// compare is constant-time (`Mac::verify_slice`); the prefix/traversal check is belt-and-braces (our
/// own signer never emits such a path, but it makes the proxy's safety self-evident and guards any
/// future caller).
pub fn verify(key: &[u8], token: &str) -> Option<String> {
    let (encoded, tag_hex) = token.split_once('.')?;
    let signature = hex::decode(tag_hex).ok()?;
    let path = String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()?;

    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    mac.update(path.as_bytes());
    mac.verify_slice(&signature).ok()?;

    if !path.starts_with(MEDIA_PREFIX) || path.contains("..") {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"tenant-webhook-secret";
    const PATH: &str = "statics/media/abc123.jpg";

    #[test]
    fn sign_then_verify_roundtrips() {
        let token = sign(KEY, PATH);
        // Two segments: base64url(path) "." hex(mac).
        assert_eq!(token.matches('.').count(), 1);
        assert_eq!(verify(KEY, &token).as_deref(), Some(PATH));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let token = sign(KEY, PATH);
        assert!(verify(b"other-secret", &token).is_none());
    }

    #[test]
    fn verify_rejects_a_tampered_token() {
        let token = sign(KEY, PATH);
        // Flip the last hex char of the tag — a still-well-formed token that no longer verifies.
        let mut tampered = token.clone();
        let last = tampered.pop().unwrap();
        tampered.push(if last == '0' { '1' } else { '0' });
        assert!(verify(KEY, &tampered).is_none());
        // Garbage shapes are rejected too (no dot, non-hex tag, non-base64 path).
        assert!(verify(KEY, "no-dot-here").is_none());
        assert!(verify(KEY, "AAAA.nothex").is_none());
    }

    #[test]
    fn verify_rejects_path_traversal_even_when_signed() {
        // A correctly-signed token whose path escapes the media dir must still be refused.
        let evil = sign(KEY, "statics/media/../../etc/passwd");
        assert!(verify(KEY, &evil).is_none());
        // …and a correctly-signed path outside the media dir entirely.
        let outside = sign(KEY, "etc/passwd");
        assert!(verify(KEY, &outside).is_none());
    }
}
