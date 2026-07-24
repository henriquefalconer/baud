// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// baud-identity — workload identity
//
// Mints ed25519-signed JWTs for tape agents and per-node identities.
// - Token subject: baud://tape/<sandbox-id>/run/<run-id>[/node/<i>]
// - TTL: 10 minutes, renewed before expiry
// - Signing via ed25519-dalek; encode/verify via jsonwebtoken
// - Tokens held as SecretString, never bare String
// - The server is the sole trust root

use std::time::{SystemTime, UNIX_EPOCH};
use baud_secret::SecretString;
use ed25519_dalek::{SigningKey, VerifyingKey};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

pub const TOKEN_TTL_SECS: u64 = 600; // 10 minutes
pub const RENEW_BEFORE_EXPIRY_SECS: u64 = 60; // renew 1 minute before expiry

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("jwt error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("invalid signing key: {0}")]
    InvalidSigningKey(String),
    #[error("invalid verifying key: {0}")]
    InvalidVerifyingKey(String),
    #[error("clock error")]
    Clock,
}

// ---------------------------------------------------------------------------
// Claims
// ---------------------------------------------------------------------------

/// Standard JWT claims for a baud identity token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject: baud://tape/<sandbox-id>/run/<run-id>[/node/<i>]
    pub sub: String,
    /// Issued-at (unix seconds)
    pub iat: u64,
    /// Expiry (unix seconds)
    pub exp: u64,
    /// Optional node index
    pub node: Option<u16>,
}

// ---------------------------------------------------------------------------
// RootKey — the server's signing authority
// ---------------------------------------------------------------------------

/// The server's signing key pair. Held in-memory; private bytes are SecretString.
pub struct RootKey {
    signing_key_bytes: SecretString,
    verifying_key: VerifyingKey,
}

impl RootKey {
    /// Create a new root key from a base64-encoded 32-byte ed25519 seed.
    pub fn from_seed_b64(seed_b64: &str) -> Result<Self, IdentityError> {
        let bytes = decode_b64(seed_b64)
            .map_err(|e| IdentityError::InvalidSigningKey(e))?;
        if bytes.len() != 32 {
            return Err(IdentityError::InvalidSigningKey(
                format!("expected 32 bytes, got {}", bytes.len()),
            ));
        }
        let seed: [u8; 32] = bytes.try_into().unwrap();
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();
        Ok(RootKey {
            signing_key_bytes: baud_secret::Secret::new(seed_b64.to_owned()),
            verifying_key,
        })
    }

    /// Generate a fresh random root key (for testing / first init).
    pub fn generate() -> Result<(Self, SecretString), IdentityError> {
        use ed25519_dalek::SigningKey as Sk;
        let mut rng = rand_os();
        let sk = Sk::generate(&mut rng);
        let vk = sk.verifying_key();
        let seed_bytes = sk.to_bytes();
        let seed_b64 = encode_b64(&seed_bytes);
        let secret = baud_secret::Secret::new(seed_b64.clone());
        let root = RootKey {
            signing_key_bytes: baud_secret::Secret::new(seed_b64),
            verifying_key: vk,
        };
        Ok((root, secret))
    }

    /// Mint a JWT for a tape agent (no node).
    pub fn mint_tape_token(
        &self,
        sandbox_id: &str,
        run_id: &str,
    ) -> Result<SecretString, IdentityError> {
        let sub = format!("baud://tape/{sandbox_id}/run/{run_id}");
        self.mint(sub, None)
    }

    /// Mint a JWT for a specific node within a run.
    pub fn mint_node_token(
        &self,
        sandbox_id: &str,
        run_id: &str,
        node: u16,
    ) -> Result<SecretString, IdentityError> {
        let sub = format!("baud://tape/{sandbox_id}/run/{run_id}/node/{node}");
        self.mint(sub, Some(node))
    }

    fn mint(&self, sub: String, node: Option<u16>) -> Result<SecretString, IdentityError> {
        let now = unix_now()?;
        let claims = Claims {
            sub,
            iat: now,
            exp: now + TOKEN_TTL_SECS,
            node,
        };

        // Reconstruct signing key from stored bytes
        let bytes = decode_b64(self.signing_key_bytes.expose())
            .map_err(|e| IdentityError::InvalidSigningKey(e))?;
        let seed: [u8; 32] = bytes.try_into()
            .map_err(|_| IdentityError::InvalidSigningKey("bad length".into()))?;
        let signing_key = SigningKey::from_bytes(&seed);
        let pk_bytes = signing_key.verifying_key().to_bytes();

        let header = Header::new(Algorithm::EdDSA);
        let enc_key = EncodingKey::from_ed_der(&der_from_ed25519_pkcs8(&seed, &pk_bytes));
        let token = jsonwebtoken::encode(&header, &claims, &enc_key)?;
        Ok(baud_secret::Secret::new(token))
    }

    /// Verify a JWT and return its claims.
    pub fn verify(&self, token: &str) -> Result<Claims, IdentityError> {
        // jsonwebtoken's from_ed_der for decoding takes the raw 32-byte public key
        // (not SPKI; despite the name, ring's UnparsedPublicKey::new with ED25519
        //  expects the raw key bytes for ed25519)
        let pk_bytes = self.verifying_key.to_bytes();
        let dec_key = DecodingKey::from_ed_der(&pk_bytes);
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.validate_exp = true;
        let data = jsonwebtoken::decode::<Claims>(token, &dec_key, &validation)?;
        Ok(data.claims)
    }

    /// Return the verifying key bytes (public key, 32 bytes).
    pub fn verifying_key_bytes(&self) -> [u8; 32] {
        self.verifying_key.to_bytes()
    }
}

// ---------------------------------------------------------------------------
// Token helpers
// ---------------------------------------------------------------------------

/// Check if a token should be renewed (within RENEW_BEFORE_EXPIRY_SECS of expiry).
pub fn should_renew(claims: &Claims) -> bool {
    let now = unix_now().unwrap_or(0);
    claims.exp.saturating_sub(now) < RENEW_BEFORE_EXPIRY_SECS
}

// ---------------------------------------------------------------------------
// DER encoding helpers for ed25519
//
// jsonwebtoken's from_ed_der expects:
//   - private: PKCS#8 OneAsymmetricKey (RFC 5958)
//   - public:  SubjectPublicKeyInfo (RFC 5480)
//
// We build these manually to avoid adding extra deps.
//
// ed25519 OID: 1.3.101.112 → 06 03 2B 65 70
//
// PKCS#8 v1 for ed25519:
//   SEQUENCE {
//     INTEGER 0                        (version = v1)
//     SEQUENCE { OID 1.3.101.112 }     (algorithmIdentifier)
//     OCTET STRING {                   (privateKey)
//       OCTET STRING { <32-byte seed> }
//     }
//   }
//
// SPKI for ed25519:
//   SEQUENCE {
//     SEQUENCE { OID 1.3.101.112 }     (algorithmIdentifier)
//     BIT STRING 0x00 <32-byte pubkey> (subjectPublicKey)
//   }
// ---------------------------------------------------------------------------

/// ed25519 algorithm identifier OID bytes (already in TLV form: 06 03 2B 65 70)
const ED25519_OID_TLV: &[u8] = &[0x06, 0x03, 0x2B, 0x65, 0x70];

/// Build a PKCS#8 v1 DER for an ed25519 private key (seed only, 32 bytes).
fn der_from_ed25519_pkcs8(seed: &[u8; 32], _pk: &[u8; 32]) -> Vec<u8> {
    // inner: OCTET STRING { seed } — the CurvePrivateKey
    let inner_os = der_tlv(0x04, seed);
    // privateKey: OCTET STRING { inner_os }
    let private_key_os = der_tlv(0x04, &inner_os);
    // algorithmIdentifier: SEQUENCE { OID }
    let alg_id = der_tlv(0x30, ED25519_OID_TLV);
    // version: INTEGER 0
    let version = &[0x02u8, 0x01, 0x00];
    // outer SEQUENCE
    let body: Vec<u8> = [version, alg_id.as_slice(), private_key_os.as_slice()].concat();
    der_tlv(0x30, &body)
}

/// Build a SubjectPublicKeyInfo DER for an ed25519 public key (32 bytes).
fn der_from_ed25519_spki(pk: &[u8; 32]) -> Vec<u8> {
    // algorithmIdentifier: SEQUENCE { OID }
    let alg_id = der_tlv(0x30, ED25519_OID_TLV);
    // BIT STRING: unused-bits byte (0x00) + pk
    let mut bit_str_body = vec![0x00u8];
    bit_str_body.extend_from_slice(pk);
    let bit_str = der_tlv(0x03, &bit_str_body);
    // outer SEQUENCE
    let body: Vec<u8> = [alg_id.as_slice(), bit_str.as_slice()].concat();
    der_tlv(0x30, &body)
}

/// Build a DER TLV (tag, length, value).
fn der_tlv(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = value.len();
    if len < 0x80 {
        out.push(len as u8);
    } else if len < 0x100 {
        out.push(0x81);
        out.push(len as u8);
    } else {
        out.push(0x82);
        out.push((len >> 8) as u8);
        out.push(len as u8);
    }
    out.extend_from_slice(value);
    out
}

// ---------------------------------------------------------------------------
// Util
// ---------------------------------------------------------------------------

fn unix_now() -> Result<u64, IdentityError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| IdentityError::Clock)
}

fn decode_b64(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    base64_decode(s).map_err(|e| format!("base64 decode: {e}"))
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    // Simple base64 decoder — use stdlib-adjacent approach
    // We use the base64 alphabet directly
    static TABLE: &[u8; 128] = &{
        let mut t = [255u8; 128];
        let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0usize;
        while i < 64 {
            t[chars[i] as usize] = i as u8;
            i += 1;
        }
        t
    };
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let a = *TABLE.get(bytes[i] as usize).filter(|&&x| x != 255)
            .ok_or_else(|| format!("invalid char at {i}"))? as u32;
        let b = *TABLE.get(bytes[i+1] as usize).filter(|&&x| x != 255)
            .ok_or_else(|| format!("invalid char at {}", i+1))? as u32;
        let c = *TABLE.get(bytes[i+2] as usize).filter(|&&x| x != 255)
            .ok_or_else(|| format!("invalid char at {}", i+2))? as u32;
        let d = *TABLE.get(bytes[i+3] as usize).filter(|&&x| x != 255)
            .ok_or_else(|| format!("invalid char at {}", i+3))? as u32;
        out.push(((a << 2) | (b >> 4)) as u8);
        out.push(((b << 4) | (c >> 2)) as u8);
        out.push(((c << 6) | d) as u8);
        i += 4;
    }
    let rem = bytes.len() - i;
    if rem == 2 {
        let a = *TABLE.get(bytes[i] as usize).filter(|&&x| x != 255)
            .ok_or("invalid char")? as u32;
        let b = *TABLE.get(bytes[i+1] as usize).filter(|&&x| x != 255)
            .ok_or("invalid char")? as u32;
        out.push(((a << 2) | (b >> 4)) as u8);
    } else if rem == 3 {
        let a = *TABLE.get(bytes[i] as usize).filter(|&&x| x != 255)
            .ok_or("invalid char")? as u32;
        let b = *TABLE.get(bytes[i+1] as usize).filter(|&&x| x != 255)
            .ok_or("invalid char")? as u32;
        let c = *TABLE.get(bytes[i+2] as usize).filter(|&&x| x != 255)
            .ok_or("invalid char")? as u32;
        out.push(((a << 2) | (b >> 4)) as u8);
        out.push(((b << 4) | (c >> 2)) as u8);
    }
    Ok(out)
}

fn encode_b64(bytes: &[u8]) -> String {
    static CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() * 4 + 2) / 3);
    let mut i = 0;
    while i + 2 < bytes.len() {
        let a = bytes[i] as u32;
        let b = bytes[i+1] as u32;
        let c = bytes[i+2] as u32;
        out.push(CHARS[((a >> 2) & 0x3F) as usize] as char);
        out.push(CHARS[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(CHARS[(((b & 0xF) << 2) | (c >> 6)) as usize] as char);
        out.push(CHARS[(c & 0x3F) as usize] as char);
        i += 3;
    }
    match bytes.len() - i {
        1 => {
            let a = bytes[i] as u32;
            out.push(CHARS[((a >> 2) & 0x3F) as usize] as char);
            out.push(CHARS[(((a & 3) << 4)) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let a = bytes[i] as u32;
            let b = bytes[i+1] as u32;
            out.push(CHARS[((a >> 2) & 0x3F) as usize] as char);
            out.push(CHARS[(((a & 3) << 4) | (b >> 4)) as usize] as char);
            out.push(CHARS[(((b & 0xF) << 2)) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

fn rand_os() -> impl rand::RngCore + rand::CryptoRng {
    rand::rngs::OsRng
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_verify() {
        let (root, _secret) = RootKey::generate().expect("generate");
        let token = root.mint_tape_token("sb-1", "run-1").expect("mint");
        let claims = root.verify(token.expose()).expect("verify");
        assert_eq!(claims.sub, "baud://tape/sb-1/run/run-1");
        assert!(claims.exp > claims.iat);
        assert_eq!(claims.node, None);
    }

    #[test]
    fn node_token_subject() {
        let (root, _) = RootKey::generate().expect("generate");
        let token = root.mint_node_token("sb-1", "run-1", 3).expect("mint");
        let claims = root.verify(token.expose()).expect("verify");
        assert_eq!(claims.sub, "baud://tape/sb-1/run/run-1/node/3");
        assert_eq!(claims.node, Some(3));
    }

    #[test]
    fn tampered_token_rejected() {
        let (root, _) = RootKey::generate().expect("generate");
        let token = root.mint_tape_token("sb-1", "run-1").expect("mint");
        // tamper: flip last char
        let mut bad = token.expose().clone();
        let last = bad.pop().unwrap();
        bad.push(if last == 'A' { 'B' } else { 'A' });
        assert!(root.verify(&bad).is_err());
    }

    #[test]
    fn should_renew_expired() {
        let claims = Claims {
            sub: "test".into(),
            iat: 0,
            exp: 0, // already expired
            node: None,
        };
        assert!(should_renew(&claims));
    }

    #[test]
    fn b64_roundtrip() {
        let original = b"hello world this is a 32-byte kk";
        let encoded = encode_b64(original);
        let decoded = base64_decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }
}
