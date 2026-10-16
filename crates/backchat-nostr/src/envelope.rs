//! The private-envelope protocol. Port of `NostrProtocol.swift`.
//!
//! Layering (kinds reuse NIP-17/59 numbers but are NOT NIP-compatible):
//! 1. rumor: unsigned kind-14 event (pubkey, created_at = real timestamp,
//!    content = plaintext message)
//! 2. seal: kind-13 event signed by the SENDER'S identity key; content is
//!    `v2:`-encrypted rumor JSON
//! 3. gift wrap: kind-1059 event signed by a one-time key; tags = the
//!    recipient `p`; content is `v2:`-encrypted seal JSON
//!
//! `v2:` = base64url(nonce24 ‖ ciphertext ‖ tag16) under
//! XChaCha20-Poly1305 with keys from secp256k1 ECDH + HKDF-SHA256.
//! Outer timestamps are randomized ±15 min; the real timestamp is inside
//! the rumor.

use crate::crypto::{private_envelope_key, shared_secret_with_parity, xchacha_open, xchacha_seal, CryptoError};
use crate::event::{kinds, EventError, NostrEvent};
use base64::Engine;
use rand::Rng;

/// Bound work before base64-decoding either encrypted layer, and before
/// parsing each decrypted nested JSON layer (upstream: 64 KiB).
pub const MAX_PRIVATE_ENVELOPE_CIPHERTEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("malformed envelope: {0}")]
    InvalidEvent(#[from] EventError),
    #[error("invalid ciphertext: {0}")]
    InvalidCiphertext(#[from] CryptoError),
    #[error("crypto stack error: {0}")]
    Crypto(#[from] secp256k1::Error),
    #[error("plaintext layer is not valid UTF-8 or JSON")]
    MalformedLayer,
}

/// An authenticated private message after full decryption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecryptedPrivateMessage {
    pub content: String,
    /// The seal signer's pubkey (the authenticated sender).
    pub sender_pubkey: String,
    /// The rumor's timestamp: the real message time (outer is randomized).
    pub timestamp: i64,
}

/// Build a BitChat private envelope ready to publish to relays.
pub fn create_private_message(
    content: &str,
    recipient_pubkey_hex: &str,
    sender: &secp256k1::Keypair,
    secp: &secp256k1::Secp256k1<secp256k1::SignOnly>,
) -> Result<NostrEvent, EnvelopeError> {
    // 1. Rumor (unsigned inner event).
    let rumor = NostrEvent::unsigned(
        hex::encode(sender.x_only_public_key().0.serialize()),
        now_secs(),
        kinds::DM,
        Vec::new(),
        content.to_string(),
    );

    // 2. Seal: encrypted to the recipient, signed with the sender's real key.
    let sealed = create_seal(&rumor, recipient_pubkey_hex, sender, secp)?;

    // 3. Gift wrap under a fresh one-time key.
    create_gift_wrap(sealed, recipient_pubkey_hex)
}

pub fn create_seal(
    rumor: &NostrEvent,
    recipient_pubkey_hex: &str,
    sender: &secp256k1::Keypair,
    secp: &secp256k1::Secp256k1<secp256k1::SignOnly>,
) -> Result<NostrEvent, EnvelopeError> {
    let encrypted = encrypt(rumor_json(rumor)?, recipient_pubkey_hex, sender)?;
    let seal = NostrEvent::unsigned(
        hex::encode(sender.x_only_public_key().0.serialize()),
        randomized_timestamp(),
        kinds::SEAL,
        Vec::new(),
        encrypted,
    );
    Ok(seal.sign(sender, secp))
}

pub fn create_gift_wrap(seal: NostrEvent, recipient_pubkey_hex: &str) -> Result<NostrEvent, EnvelopeError> {
    let mut wrap_rng = rand::rng();
    let wrap_key = secp256k1::Keypair::new(&secp256k1::Secp256k1::signing_only(), &mut wrap_rng);
    let encrypted = encrypt(seal_json(&seal)?, recipient_pubkey_hex, &wrap_key)?;
    let wrap = NostrEvent::unsigned(
        hex::encode(wrap_key.x_only_public_key().0.serialize()),
        randomized_timestamp(),
        kinds::GIFT_WRAP,
        vec![vec!["p".into(), recipient_pubkey_hex.to_string()]],
        encrypted,
    );
    Ok(wrap.sign(&wrap_key, &secp256k1::Secp256k1::signing_only()))
}

/// Decrypt a received private envelope with full authentication checks:
/// envelope binding, seal signature, rumor/seal signer match, tag shape.
pub fn decrypt_private_message(
    gift_wrap: &NostrEvent,
    recipient_pubkey_hex: &str,
    recipient_secret: &secp256k1::SecretKey,
) -> Result<DecryptedPrivateMessage, EnvelopeError> {
    // 0. Validate the untrusted outer envelope before any decryption work.
    if gift_wrap.content.len() > MAX_PRIVATE_ENVELOPE_CIPHERTEXT_BYTES {
        return Err(CryptoError::TooLong.into());
    }
    if gift_wrap.kind != kinds::GIFT_WRAP
        || gift_wrap.tags != [["p".into(), recipient_pubkey_hex.to_string()]]
        || !gift_wrap.is_valid_signature()
    {
        return Err(EnvelopeError::InvalidEvent(EventError::Malformed("outer envelope")));
    }

    // 1. Unwrap.
    let seal_json = decrypt(&gift_wrap.content, &gift_wrap.pubkey, recipient_secret)?;
    let seal: NostrEvent = serde_json::from_str(&seal_json).map_err(|_| EnvelopeError::MalformedLayer)?;

    // 2. Authenticate the seal: signed by the sender's real identity key.
    if seal.kind != kinds::SEAL || !seal.tags.is_empty() || !seal.is_valid_signature() {
        return Err(EnvelopeError::InvalidEvent(EventError::Malformed("seal")));
    }

    // 3. Open the seal.
    let rumor_json = decrypt(&seal.content, &seal.pubkey, recipient_secret)?;
    let rumor: NostrEvent =
        serde_json::from_str(&rumor_json).map_err(|_| EnvelopeError::MalformedLayer)?;

    // 4. Sender authentication comes from the seal: the rumor's claimed
    //    sender must match the seal signer.
    if rumor.kind != kinds::DM
        || !rumor.sig.is_none()
        || seal.pubkey != rumor.pubkey
        || !valid_inner_message_tags(&rumor.tags, recipient_pubkey_hex)
    {
        return Err(EnvelopeError::InvalidEvent(EventError::Malformed("rumor")));
    }

    Ok(DecryptedPrivateMessage {
        content: rumor.content,
        sender_pubkey: seal.pubkey,
        timestamp: rumor.created_at,
    })
}

/// Released iOS envelopes use no inner tags; current Android places exactly
/// the recipient `p` tag. Accept only those two shapes.
fn valid_inner_message_tags(tags: &[Vec<String>], recipient_pubkey_hex: &str) -> bool {
    tags.is_empty() || tags == [["p".into(), recipient_pubkey_hex.to_string()]]
}

/// `v2:` base64url(nonce24 ‖ ciphertext ‖ tag).
pub fn encrypt(
    plaintext: String,
    recipient_pubkey_hex: &str,
    sender_key: &secp256k1::Keypair,
) -> Result<String, EnvelopeError> {
    let recipient_xonly: [u8; 32] = hex::decode(recipient_pubkey_hex)
        .ok()
        .and_then(|d| <[u8; 32]>::try_from(d).ok())
        .ok_or(CryptoError::InvalidKey)?;
    let ss = shared_secret_with_parity(&sender_key.secret_key(), &recipient_xonly, secp256k1::Parity::Even);
    let key = private_envelope_key(&ss);

    let mut nonce24 = [0u8; 24];
    rand::rng().fill(&mut nonce24);
    let (ct, tag) = xchacha_seal(plaintext.as_bytes(), &key, &nonce24)?;

    let mut combined = Vec::with_capacity(24 + ct.len() + 16);
    combined.extend_from_slice(&nonce24);
    combined.extend_from_slice(&ct);
    combined.extend_from_slice(&tag);
    Ok(format!("v2:{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(combined)))
}

/// Inverse of `encrypt`. `sender_pubkey_hex` is the x-only key from the
/// envelope layer's pubkey field.
pub fn decrypt(
    ciphertext: &str,
    sender_pubkey_hex: &str,
    recipient_secret: &secp256k1::SecretKey,
) -> Result<String, EnvelopeError> {
    if ciphertext.len() > MAX_PRIVATE_ENVELOPE_CIPHERTEXT_BYTES || !ciphertext.starts_with("v2:") {
        return Err(CryptoError::TooLong.into());
    }
    let data = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&ciphertext[3..])
        .map_err(|_| CryptoError::TooLong)?;
    if data.len() <= 24 + 16 {
        return Err(CryptoError::TooLong.into());
    }
    let sender_xonly: [u8; 32] = hex::decode(sender_pubkey_hex)
        .ok()
        .and_then(|d| <[u8; 32]>::try_from(d).ok())
        .ok_or(CryptoError::InvalidKey)?;

    let nonce24: [u8; 24] = data[..24].try_into().unwrap();
    let rest = &data[24..];
    let (ct, tag) = rest.split_at(rest.len() - 16);
    let mut tag16 = [0u8; 16];
    tag16.copy_from_slice(tag);

    // Try decryption with even-Y then odd-Y parses of the sender's x-only
    // pubkey: the assumed parity changes the shared point, so exactly one
    // parse yields the sender's key (upstream does the same two attempts).
    let try_parse = |assumed: secp256k1::Parity| -> Result<Vec<u8>, CryptoError> {
        let ss = shared_secret_with_parity(recipient_secret, &sender_xonly, assumed);
        let key = private_envelope_key(&ss);
        xchacha_open(ct, &tag16, &key, &nonce24)
    };
    let pt = try_parse(secp256k1::Parity::Even)
        .or_else(|_| try_parse(secp256k1::Parity::Odd))?;

    String::from_utf8(pt).map_err(|_| EnvelopeError::MalformedLayer.into())
}

fn rumor_json(rumor: &NostrEvent) -> Result<String, EnvelopeError> {
    serde_json::to_string(rumor).map_err(|_| EnvelopeError::MalformedLayer)
}

fn seal_json(seal: &NostrEvent) -> Result<String, EnvelopeError> {
    serde_json::to_string(seal).map_err(|_| EnvelopeError::MalformedLayer)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Outer seal/envelope timestamps are randomized ±15 min so relays cannot
/// correlate by timing; the real timestamp rides inside the rumor.
fn randomized_timestamp() -> i64 {
    let offset = rand::rng().random_range(-900i64..=900);
    now_secs() + offset
}
