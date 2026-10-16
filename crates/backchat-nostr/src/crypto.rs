//! The BitChat private-envelope primitives. Ports of
//! `XChaCha20Poly1305Compat.swift` (which is just the standard XChaCha20
//! construction) and the key-derivation half of `NostrProtocol.swift`.
//!
//! Not NIP-44: the HKDF info string reuses the historical "nip44-v2" label
//! for wire compatibility, but the schedule is HKDF-SHA256 over the raw
//! secp256k1 ECDH shared secret with an empty salt.

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use hkdf::Hkdf;
use sha2::Sha256;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("invalid key length (expected 32)")]
    InvalidKey,
    #[error("invalid nonce length (expected 24)")]
    InvalidNonce,
    #[error("decryption failed (bad tag or wrong key)")]
    DecryptFailed,
    #[error("ciphertext too long")]
    TooLong,
    #[error("secp256k1 error: {0}")]
    Secp(#[from] secp256k1::Error),
}

/// Raw secp256k1 ECDH shared secret: the 33-byte compressed point
/// (0x02/0x03 prefix ‖ x): upstream's P256K wrapper returns the shared
/// secret in `.compressed` format, and the fixtures prove the HKDF input is
/// the full 33 bytes (parity included), not the bare x-coordinate.
///
/// The point (and therefore the prefix) depends on the parity assumed when
/// parsing an x-only key: senders parse the recipient key as even; decoders
/// try even, then odd (mirroring upstream's attemptDecrypt).
pub fn shared_secret_with_parity(
    secret: &secp256k1::SecretKey,
    pubkey_xonly: &[u8; 32],
    assumed: secp256k1::Parity,
) -> [u8; 33] {
    let public = secp256k1::PublicKey::from_x_only_public_key(
        secp256k1::XOnlyPublicKey::from_byte_array(*pubkey_xonly).expect("valid x-only key"),
        assumed,
    );
    let point = secp256k1::ecdh::shared_secret_point(&public, secret);
    let (x, y) = point.split_at(32);
    let parity = if y[31] & 1 == 1 {
        secp256k1::Parity::Odd
    } else {
        secp256k1::Parity::Even
    };
    let mut out = [0u8; 33];
    out[0] = match parity {
        secp256k1::Parity::Even => 0x02,
        secp256k1::Parity::Odd => 0x03,
    };
    out[1..].copy_from_slice(x);
    out
}

/// HKDF-SHA256 with empty salt and the historical "nip44-v2" info label.
pub fn private_envelope_key(shared_secret: &[u8; 33]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, shared_secret);
    let mut okm = [0u8; 32];
    hk.expand(b"nip44-v2", &mut okm).expect("32-byte output is valid for HKDF-SHA256");
    okm
}

/// Standard XChaCha20-Poly1305 seal; returns ciphertext and tag separately
/// (upstream's SealBox shape).
pub fn xchacha_seal(
    plaintext: &[u8],
    key: &[u8; 32],
    nonce24: &[u8; 24],
) -> Result<(Vec<u8>, [u8; 16]), CryptoError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let sealed = cipher
        .encrypt(
            XNonce::from_slice(nonce24),
            Payload { msg: plaintext, aad: &[] },
        )
        .map_err(|_| CryptoError::DecryptFailed)?;
    let ct_len = sealed.len() - 16;
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&sealed[ct_len..]);
    Ok((sealed[..ct_len].to_vec(), tag))
}

/// Standard XChaCha20-Poly1305 open.
pub fn xchacha_open(
    ciphertext: &[u8],
    tag: &[u8; 16],
    key: &[u8; 32],
    nonce24: &[u8; 24],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut combined = Vec::with_capacity(ciphertext.len() + 16);
    combined.extend_from_slice(ciphertext);
    combined.extend_from_slice(tag);
    cipher
        .decrypt(
            XNonce::from_slice(nonce24),
            Payload { msg: combined.as_slice(), aad: &[] },
        )
        .map_err(|_| CryptoError::DecryptFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xchacha_roundtrip() {
        let key = [7u8; 32];
        let nonce = [9u8; 24];
        let pt = b"hello, geohash channel";
        let (ct, tag) = xchacha_seal(pt, &key, &nonce).unwrap();
        assert_eq!(ct.len(), pt.len());
        assert_eq!(xchacha_open(&ct, &tag, &key, &nonce).unwrap(), pt);
    }

    #[test]
    fn xchacha_wrong_key_fails() {
        let (ct, tag) = xchacha_seal(b"x", &[1u8; 32], &[2u8; 24]).unwrap();
        assert!(xchacha_open(&ct, &tag, &[3u8; 32], &[2u8; 24]).is_err());
    }
}
