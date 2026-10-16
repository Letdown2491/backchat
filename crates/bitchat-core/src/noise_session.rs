//! Noise_XX_25519_ChaChaPoly_SHA256 hand-rolled to match upstream's own
//! hand-rolled implementation (`bitchat/Noise/NoiseProtocol.swift`), because
//! the deployed transport frame is *not* standard Noise:
//!
//! - Transport wire frame: `4-byte big-endian nonce ‖ ciphertext ‖ 16-byte tag`.
//! - The ChaCha20 nonce is the standard Noise IETF construction
//!   `00000000 ‖ LE64(counter)`, but the counter is *carried in the frame*
//!   (big-endian, low 32 bits — senders stop above `u32::MAX - 1`) instead of
//!   being implicit, and the receiver applies a 1024-bit sliding replay
//!   window rather than requiring in-order delivery.
//! - Transport keys come from the standard `Split()`: `HKDF(ck, empty, 2)`;
//!   the initiator sends with key 1, receives with key 2.
//!
//! Handshake messages themselves are standard XX with empty payloads:
//! msg1 `-> e` (32 B), msg2 `<- e, ee, s, es` (80 B), msg3 `-> s, se` (48 B).
//! snow cannot express the transport frame above, so the pattern is done by
//! hand here; `tests/noise_session.rs` cross-checks every step against snow,
//! which is the fixture-verified reference.
//!
//! Upstream sources of truth: `Noise/NoiseProtocol.swift` (symmetrics +
//! cipher state) and `Noise/NoiseSession.swift` (`startHandshake`,
//! `getTransportCiphers(useExtractedNonce: true)`).

use chacha20poly1305::aead::OsRng;
use chacha20poly1305::{AeadInPlace, ChaCha20Poly1305, Key, KeyInit, Nonce};
use sha2::{Digest, Sha256};
use thiserror::Error;
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret as XStaticSecret};

use crate::noise::StaticIdentity;

pub const HANDSHAKE_PROTOCOL_NAME: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

const TAG_LEN: usize = 16;
const NONCE_PREFIX_LEN: usize = 4;
/// 1024-bit sliding window, mirroring `NoiseCipherState.REPLAY_WINDOW_SIZE`.
const REPLAY_WINDOW_BITS: usize = 1024;
const REPLAY_WINDOW_BYTES: usize = REPLAY_WINDOW_BITS / 8;

/// Send-counter guard from upstream (`nonce <= UInt32.max - 1`).
const MAX_SEND_NONCE: u64 = u32::MAX as u64 - 1;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("handshake message too short ({got} bytes, expected at least {min})")]
    TooShort { got: usize, min: usize },
    #[error("handshake failed: authentication tag mismatch")]
    AuthFailed,
    #[error("handshake already finished")]
    Finished,
    #[error("handshake in wrong state for this step")]
    WrongStep,
    #[error("nonce counter exhausted (rekey required)")]
    CounterExhausted,
    #[error("replay detected or nonce too old: {0}")]
    Replay(u64),
}

// ---------------------------------------------------------------------------
// Noise symmetrics (HMAC-SHA256 based, per the Noise spec)
// ---------------------------------------------------------------------------

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    // RFC 2104 with SHA-256 (block size 64).
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let ih = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(ih);
    outer.finalize().into()
}

/// Noise `HKDF(ck, ikm, 2)`.
fn hkdf2(ck: &[u8; 32], ikm: &[u8]) -> ([u8; 32], [u8; 32]) {
    let prk = hmac_sha256(ck, ikm);
    let o1 = hmac_sha256(&prk, &[0x01]);
    let mut buf = [0u8; 33];
    buf[..32].copy_from_slice(&o1);
    buf[32] = 0x02;
    let o2 = hmac_sha256(&prk, &buf);
    (o1, o2)
}

/// Noise `MIX_KEY(ikm)`.
fn mix_key(ck: &mut [u8; 32], ikm: &[u8]) -> [u8; 32] {
    let (new_ck, k) = hkdf2(ck, ikm);
    *ck = new_ck;
    k
}

/// Noise `MIX_HASH(data)`.
fn mix_hash(h: &mut [u8; 32], data: &[u8]) {
    let mut hasher = Sha256::new();
    hasher.update(h.as_slice());
    hasher.update(data);
    *h = hasher.finalize().into();
}

/// Keyed AEAD with a sequential per-key nonce (upstream `NoiseCipherState`):
/// the nonce starts at 0 when `MIX_KEY` installs a new key and increments on
/// EVERY use — including the empty-payload frames between tokens. This is
/// why msg3's static is sealed at nonce 1, not 0.
fn handshake_seal(key: &[u8; 32], h: &[u8; 32], nonce: u64, plaintext: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let mut n = [0u8; 12];
    n[4..12].copy_from_slice(&nonce.to_le_bytes());
    let mut buf = plaintext.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(&n), h, &mut buf)
        .expect("chacha20poly1305 seal cannot fail");
    buf.extend_from_slice(&tag);
    buf
}

/// Inverse of [`handshake_seal`]; returns the plaintext.
fn handshake_open(key: &[u8; 32], h: &[u8; 32], nonce: u64, ct: &[u8]) -> Result<Vec<u8>, SessionError> {
    if ct.len() < TAG_LEN {
        return Err(SessionError::TooShort { got: ct.len(), min: TAG_LEN });
    }
    let (body, tag) = ct.split_at(ct.len() - TAG_LEN);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let mut n = [0u8; 12];
    n[4..12].copy_from_slice(&nonce.to_le_bytes());
    let mut buf = body.to_vec();
    cipher
        .decrypt_in_place_detached(Nonce::from_slice(&n), h, &mut buf, tag.into())
        .map_err(|_| SessionError::AuthFailed)?;
    Ok(buf)
}

/// Standard Noise IETF nonce: `00000000 ‖ LE64(counter)`.
fn chacha_nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..12].copy_from_slice(&counter.to_le_bytes());
    Nonce::from_slice(&n).to_owned() // safe: 12-byte array
}

// ---------------------------------------------------------------------------
// Transport cipher (the deployed, extracted-nonce frame)
// ---------------------------------------------------------------------------

/// 1024-bit sliding replay window matching upstream `markNonceAsSeen` /
/// `isValidNonce` bit-for-bit.
#[derive(Debug, Clone)]
struct ReplayWindow {
    highest: u64,
    bits: [u8; REPLAY_WINDOW_BYTES],
}

impl ReplayWindow {
    fn new() -> Self {
        Self { highest: 0, bits: [0; REPLAY_WINDOW_BYTES] }
    }

    fn check(&self, n: u64) -> Result<(), SessionError> {
        if n > self.highest {
            return Ok(()); // always accept newer
        }
        let offset = (self.highest - n) as usize;
        if offset >= REPLAY_WINDOW_BITS {
            return Err(SessionError::Replay(n));
        }
        if self.bits[offset / 8] & (1 << (offset % 8)) != 0 {
            return Err(SessionError::Replay(n));
        }
        Ok(())
    }

    fn mark(&mut self, n: u64) {
        if n > self.highest {
            let shift = (n - self.highest) as usize;
            if shift >= REPLAY_WINDOW_BITS {
                self.bits = [0; REPLAY_WINDOW_BYTES];
            } else {
                // Shift the bitfield right by `shift` bits (older nonces age out).
                let mut new_bits = [0u8; REPLAY_WINDOW_BYTES];
                for i in (0..REPLAY_WINDOW_BYTES).rev() {
                    let src = i as isize - (shift / 8) as isize;
                    if src < 0 {
                        continue;
                    }
                    let mut byte = self.bits[src as usize] >> (shift % 8);
                    if shift % 8 != 0 && src > 0 {
                        byte |= self.bits[(src - 1) as usize] << (8 - shift % 8);
                    }
                    new_bits[i] = byte;
                }
                self.bits = new_bits;
            }
            self.highest = n;
            self.bits[0] |= 1;
        } else {
            let offset = (self.highest - n) as usize;
            self.bits[offset / 8] |= 1 << (offset % 8);
        }
    }
}

/// One direction of an established Noise session, using upstream's
/// extracted-nonce transport frame. `seal` drives the send counter; `open`
/// validates inbound frames with the replay window.
#[derive(Debug, Clone)]
pub struct TransportCipher {
    key: [u8; 32],
    send_counter: u64,
    window: ReplayWindow,
}

impl TransportCipher {
    pub fn new(key: [u8; 32]) -> Self {
        Self { key, send_counter: 0, window: ReplayWindow::new() }
    }

    /// Frame: `BE32(counter) ‖ ct ‖ tag`, ChaCha nonce `00000000 ‖ LE64(counter)`.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, SessionError> {
        let n = self.send_counter;
        if n > MAX_SEND_NONCE {
            return Err(SessionError::CounterExhausted);
        }
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.key));
        let mut buf = plaintext.to_vec();
        let tag = cipher
            .encrypt_in_place_detached(&chacha_nonce(n), &[], &mut buf)
            .expect("chacha20poly1305 seal cannot fail");
        self.send_counter += 1;
        let mut out = Vec::with_capacity(NONCE_PREFIX_LEN + buf.len() + TAG_LEN);
        out.extend_from_slice(&(n as u32).to_be_bytes());
        out.extend_from_slice(&buf);
        out.extend_from_slice(&tag);
        Ok(out)
    }

    /// Parse and open an inbound frame. The nonce comes from the frame
    /// itself (out-of-order delivery is fine within the replay window).
    pub fn open(&mut self, frame: &[u8]) -> Result<Vec<u8>, SessionError> {
        if frame.len() < NONCE_PREFIX_LEN + TAG_LEN {
            return Err(SessionError::TooShort { got: frame.len(), min: NONCE_PREFIX_LEN + TAG_LEN });
        }
        let mut prefix = [0u8; 4];
        prefix.copy_from_slice(&frame[..NONCE_PREFIX_LEN]);
        let n = u32::from_be_bytes(prefix) as u64;
        self.window.check(n)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.key));
        let body = &frame[NONCE_PREFIX_LEN..frame.len() - TAG_LEN];
        let tag = &frame[frame.len() - TAG_LEN..];
        let mut buf = body.to_vec();
        match cipher.decrypt_in_place_detached(&chacha_nonce(n), &[], &mut buf, tag.into()) {
            Ok(()) => {
                self.window.mark(n);
                Ok(buf)
            }
            Err(_) => Err(SessionError::AuthFailed),
        }
    }
}

// ---------------------------------------------------------------------------
// XX handshake state machines
// ---------------------------------------------------------------------------

/// The result of a finished XX handshake, both roles.
pub struct CompletedHandshake {
    pub send: TransportCipher,
    pub recv: TransportCipher,
    pub handshake_hash: [u8; 32],
    /// Authenticated remote Noise static public key.
    pub remote_static: [u8; 32],
}

/// Raw protocol-name bytes: the initial `ck`/`h` before the prologue mix.
fn protocol_name_h() -> [u8; 32] {
    // Noise spec: name ≤ 32 bytes → h is the name zero-padded raw. Ours is
    // exactly 32.
    let name = HANDSHAKE_PROTOCOL_NAME.as_bytes();
    let mut h = [0u8; 32];
    if name.len() <= 32 {
        h[..name.len()].copy_from_slice(name);
    } else {
        h.copy_from_slice(&Sha256::digest(name));
    }
    h
}

/// Standard Noise XX, initiator side. Drives msg1 and msg3.
pub struct HandshakeInitiator {
    identity: StaticIdentity,
    s: XStaticSecret,
    e: XStaticSecret,
    e_pub: [u8; 32],
    ck: [u8; 32],
    h: [u8; 32],
    /// Cipher key chained from msg2's `es`; encrypts msg3's `s`.
    k: [u8; 32],
    /// Per-key AEAD nonce (upstream `NoiseCipherState`): resets to 0 on
    /// every `MIX_KEY`, increments on every keyed encrypt/decrypt.
    aead_nonce: u64,
    step: u8,
}

impl HandshakeInitiator {
    pub fn new(identity: StaticIdentity) -> Self {
        let e = XStaticSecret::random_from_rng(&mut OsRng);
        let e_pub = XPublicKey::from(&e).to_bytes();
        // ck = h = raw protocol name, then the (empty) prologue is mixed
        // into h only — upstream does this unconditionally in
        // mixPreMessageKeys(), so h becomes SHA256(name).
        let mut h = protocol_name_h();
        let ck = h;
        mix_hash(&mut h, &[]);
        let s = XStaticSecret::from(identity.private_key);
        Self { identity, s, e, e_pub, ck, h, k: [0; 32], aead_nonce: 0, step: 0 }
    }

    /// msg1 `-> e` (32 bytes). The trailing `ENCRYPT_AND_HASH(empty)` with no
    /// key is a pass-through that still mixes the (empty) payload into `h`.
    pub fn step1(&mut self) -> Result<Vec<u8>, SessionError> {
        if self.step != 0 {
            return Err(SessionError::WrongStep);
        }
        self.step = 1;
        mix_hash(&mut self.h, &self.e_pub);
        mix_hash(&mut self.h, &[]); // encryptAndHash(empty) with no key
        Ok(self.e_pub.to_vec())
    }

    /// Consume msg2 `<- e, ee, s, es, payload>` (96 bytes), emit msg3
    /// `-> s, se, payload>` (64 bytes) and finish into transport ciphers.
    pub fn step3(mut self, msg2: &[u8]) -> Result<(Vec<u8>, CompletedHandshake), SessionError> {
        if self.step != 1 {
            return Err(SessionError::WrongStep);
        }
        if msg2.len() != 96 {
            return Err(SessionError::TooShort { got: msg2.len(), min: 96 });
        }
        let mut e2 = [0u8; 32];
        e2.copy_from_slice(&msg2[..32]);
        let ct_s_resp = &msg2[32..80];
        let payload_ct = &msg2[80..96];

        // <- e
        mix_hash(&mut self.h, &e2);
        // ee: DH(initiator e, responder e) — new key, nonce resets to 0
        let k = mix_key(&mut self.ck, self.e.diffie_hellman(&XPublicKey::from(e2)).as_bytes());
        self.aead_nonce = 0;
        // s: responder static, encrypted; authenticating h as AD
        let s_resp = handshake_open(&k, &self.h, self.aead_nonce, ct_s_resp)?;
        self.aead_nonce += 1;
        if s_resp.len() != 32 {
            return Err(SessionError::TooShort { got: s_resp.len(), min: 32 });
        }
        mix_hash(&mut self.h, ct_s_resp);
        let mut remote_static = [0u8; 32];
        remote_static.copy_from_slice(&s_resp);
        // es: DH(initiator e, responder s) — new key, nonce resets to 0
        self.k = mix_key(&mut self.ck, self.e.diffie_hellman(&XPublicKey::from(remote_static)).as_bytes());
        self.aead_nonce = 0;
        // trailing (empty) payload frame of msg2
        let payload_pt = handshake_open(&self.k, &self.h, self.aead_nonce, payload_ct)?;
        self.aead_nonce += 1;
        if !payload_pt.is_empty() {
            return Err(SessionError::TooShort { got: payload_pt.len(), min: 0 });
        }
        mix_hash(&mut self.h, payload_ct);

        // -> s: initiator static encrypted with the key chained from `es`
        // (second use of that key, hence the incremented nonce)
        let ct_s = handshake_seal(&self.k, &self.h, self.aead_nonce, &self.identity.public_key);
        self.aead_nonce += 1;
        mix_hash(&mut self.h, &ct_s);
        // se: DH(initiator s, responder e) — new key, nonce resets to 0
        let k_final = mix_key(&mut self.ck, self.s.diffie_hellman(&XPublicKey::from(e2)).as_bytes());
        // trailing (empty) payload frame of msg3
        let out_payload = handshake_seal(&k_final, &self.h, 0, &[]);
        mix_hash(&mut self.h, &out_payload);

        let (k1, k2) = hkdf2(&self.ck, &[]);
        Ok((
            [ct_s, out_payload].concat(),
            CompletedHandshake {
                send: TransportCipher::new(k1), // initiator sends with c1
                recv: TransportCipher::new(k2),
                handshake_hash: self.h,
                remote_static,
            },
        ))
    }
}

/// Standard Noise XX, responder side. Produces msg2, then consumes msg3.
pub struct HandshakeResponder {
    identity: StaticIdentity,
    s: XStaticSecret,
    e: XStaticSecret,
    ck: [u8; 32],
    h: [u8; 32],
    /// Cipher key chained from msg2's `es`; decrypts msg3's `s`.
    k: [u8; 32],
    /// Per-key AEAD nonce (upstream `NoiseCipherState`): resets to 0 on
    /// every `MIX_KEY`, increments on every keyed encrypt/decrypt.
    aead_nonce: u64,
    step: u8,
}

impl HandshakeResponder {
    pub fn new(identity: StaticIdentity) -> Self {
        let e = XStaticSecret::random_from_rng(&mut OsRng);
        // ck = h = raw protocol name, then the (empty) prologue is mixed
        // into h only — upstream does this unconditionally in
        // mixPreMessageKeys(), so h becomes SHA256(name).
        let mut h = protocol_name_h();
        let ck = h;
        mix_hash(&mut h, &[]);
        let s = XStaticSecret::from(identity.private_key);
        Self { identity, s, e, ck, h, k: [0; 32], aead_nonce: 0, step: 0 }
    }

    /// Consume msg1 `-> e` (32 bytes), produce msg2 `<- e, ee, s, es, payload>`
    /// (96 bytes).
    pub fn step2(&mut self, msg1: &[u8]) -> Result<Vec<u8>, SessionError> {
        if self.step != 0 {
            return Err(SessionError::WrongStep);
        }
        if msg1.len() != 32 {
            return Err(SessionError::TooShort { got: msg1.len(), min: 32 });
        }
        let mut e1 = [0u8; 32];
        e1.copy_from_slice(msg1);
        let e2_pub = XPublicKey::from(&self.e).to_bytes();

        // <- e (remote, from msg1) — msg1 is processed first
        mix_hash(&mut self.h, &e1);
        mix_hash(&mut self.h, &[]); // msg1's keyless encryptAndHash(empty)
        // -> e (ours, being sent now): the sender mixes its own e token
        mix_hash(&mut self.h, &e2_pub);
        // ee: DH(responder e, initiator e) — new key, nonce resets to 0
        let k = mix_key(&mut self.ck, self.e.diffie_hellman(&XPublicKey::from(e1)).as_bytes());
        self.aead_nonce = 0;
        // s: our encrypted static
        let ct_s = handshake_seal(&k, &self.h, self.aead_nonce, &self.identity.public_key);
        self.aead_nonce += 1;
        mix_hash(&mut self.h, &ct_s);
        // es: DH(initiator e, responder s) — new key, nonce resets to 0
        self.k = mix_key(&mut self.ck, self.s.diffie_hellman(&XPublicKey::from(e1)).as_bytes());
        self.aead_nonce = 0;
        // trailing (empty) payload frame
        let payload_ct = handshake_seal(&self.k, &self.h, self.aead_nonce, &[]);
        mix_hash(&mut self.h, &payload_ct);

        let mut msg2 = Vec::with_capacity(96);
        msg2.extend_from_slice(&e2_pub);
        msg2.extend_from_slice(&ct_s);
        msg2.extend_from_slice(&payload_ct);
        self.step = 1;
        Ok(msg2)
    }

    /// Consume msg3 `-> s, se, payload>` (64 bytes); finish into transport
    /// ciphers. The initiator static arrives *claimed* inside msg3 — the
    /// caller must check `PeerId(remote_static) == packet.sender_id` before
    /// trusting it.
    pub fn step4(mut self, msg3: &[u8]) -> Result<CompletedHandshake, SessionError> {
        if self.step != 1 {
            return Err(SessionError::WrongStep);
        }
        if msg3.len() != 64 {
            return Err(SessionError::TooShort { got: msg3.len(), min: 64 });
        }
        let ct_s = &msg3[..48];
        let payload_ct = &msg3[48..64];
        // -> s: initiator static, encrypted with the key chained from `es`
        // (second use of that key, hence the incremented nonce)
        let s_init = handshake_open(&self.k, &self.h, 1, ct_s)?;
        if s_init.len() != 32 {
            return Err(SessionError::TooShort { got: s_init.len(), min: 32 });
        }
        mix_hash(&mut self.h, ct_s);
        let mut remote_static = [0u8; 32];
        remote_static.copy_from_slice(&s_init);
        // se: DH(responder e, initiator s)
        let k_final = mix_key(&mut self.ck, self.e.diffie_hellman(&XPublicKey::from(remote_static)).as_bytes());
        // trailing (empty) payload frame
        let payload_pt = handshake_open(&k_final, &self.h, 0, payload_ct)?;
        if !payload_pt.is_empty() {
            return Err(SessionError::TooShort { got: payload_pt.len(), min: 0 });
        }
        mix_hash(&mut self.h, payload_ct);

        // Split: initiator sends with c1, so we receive with c1.
        let (k1, k2) = hkdf2(&self.ck, &[]);
        Ok(CompletedHandshake {
            send: TransportCipher::new(k2),
            recv: TransportCipher::new(k1),
            handshake_hash: self.h,
            remote_static,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::noise::StaticIdentity;

    /// Full mine↔mine exchange must derive identical handshake hashes,
    /// cross-authenticating remote statics, with working bidirectional
    /// transport (including the msg3 static sealed at per-key nonce 1).
    #[test]
    fn xx_roundtrip_self_consistent() {
        let a = StaticIdentity::generate();
        let b = StaticIdentity::generate();

        let mut init = HandshakeInitiator::new(a.clone());
        let mut resp = HandshakeResponder::new(b.clone());
        let msg1 = init.step1().unwrap();
        let msg2 = resp.step2(&msg1).unwrap();
        let (msg3, mut init_done) = init.step3(&msg2).unwrap();
        let mut resp_done = resp.step4(&msg3).unwrap();

        assert_eq!(init_done.remote_static, b.public_key);
        assert_eq!(resp_done.remote_static, a.public_key);
        assert_eq!(init_done.handshake_hash, resp_done.handshake_hash);

        let f1 = init_done.send.seal(b"ping").unwrap();
        assert_eq!(resp_done.recv.open(&f1).unwrap(), b"ping");
        let f2 = resp_done.send.seal(b"pong").unwrap();
        assert_eq!(init_done.recv.open(&f2).unwrap(), b"pong");
    }

    /// Byte-level: with identical static + ephemeral keys, my msg2 must be
    /// indistinguishable from snow's (the fixture-verified reference).
    #[test]
    fn msg2_bytes_match_snow() {
        use snow::resolvers::DefaultResolver;

        let idb = StaticIdentity::generate();
        let eph = [0x37u8; 32];

        let mut snow_hs = snow::Builder::with_resolver(
            crate::noise::PROTOCOL_NAME.parse().unwrap(),
            Box::new(DefaultResolver::default()),
        )
        .local_private_key(&idb.private_key)
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&eph)
        .build_responder()
        .unwrap();

        let mut init = HandshakeInitiator::new(StaticIdentity::generate());
        let msg1 = init.step1().unwrap();
        let mut buf = vec![0u8; 65535];
        snow_hs.read_message(&msg1, &mut buf).unwrap();
        let n = snow_hs.write_message(&[], &mut buf).unwrap();
        let snow_msg2 = buf[..n].to_vec();

        // Mine with the same static + ephemeral (private field access).
        let mut resp = HandshakeResponder::new(idb);
        resp.e = XStaticSecret::from(eph);
        let my_msg2 = resp.step2(&msg1).unwrap();

        assert_eq!(snow_msg2, my_msg2, "msg2 bytes diverge from snow");
    }

    /// Byte-level: my initiator's msg1 and msg3 must match snow's.
    #[test]
    fn msg1_msg3_bytes_match_snow() {
        use snow::resolvers::DefaultResolver;

        let ida = StaticIdentity::generate();
        let eph = [0x5au8; 32];

        let mut snow_i = snow::Builder::with_resolver(
            crate::noise::PROTOCOL_NAME.parse().unwrap(),
            Box::new(DefaultResolver::default()),
        )
        .local_private_key(&ida.private_key)
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&eph)
        .build_initiator()
        .unwrap();

        // Responder is mine (random ephemeral — msg3 is comparable because
        // both initiators consume the same msg2).
        let mut resp = HandshakeResponder::new(StaticIdentity::generate());

        let mut buf = vec![0u8; 65535];
        let n1 = snow_i.write_message(&[], &mut buf).unwrap();
        let snow_msg1 = buf[..n1].to_vec();

        let mut init = HandshakeInitiator::new(ida.clone());
        init.e = XStaticSecret::from(eph);
        init.e_pub = XPublicKey::from(&init.e).to_bytes();
        let my_msg1 = init.step1().unwrap();
        assert_eq!(snow_msg1, my_msg1, "msg1 bytes diverge");

        let my_msg2 = resp.step2(&snow_msg1).unwrap();
        let n2 = snow_i.read_message(&my_msg2, &mut buf).unwrap();
        assert_eq!(n2, 0);

        let n3 = snow_i.write_message(&[], &mut buf).unwrap();
        let snow_msg3 = buf[..n3].to_vec();

        // Replay with my initiator (same ephemeral, same msg2).
        let mut init2 = HandshakeInitiator::new(ida);
        init2.e = XStaticSecret::from(eph);
        init2.e_pub = XPublicKey::from(&init2.e).to_bytes();
        let m1 = init2.step1().unwrap();
        let (my_msg3, _done) = init2.step3(&my_msg2).unwrap();
        assert_eq!(m1, snow_msg1);
        assert_eq!(snow_msg3, my_msg3, "msg3 bytes diverge");
    }
}
