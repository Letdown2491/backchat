//! Noise session conformance. The handshake math is cross-checked against
//! snow (the fixture-verified reference, see `noise_vectors.rs`); the
//! extracted-nonce transport frame is pinned against a manual construction
//! mirroring upstream `NoiseProtocol.swift` (CryptoKit ChaChaPoly with
//! `nonce = 00000000 ‖ LE64(counter)`, frame `BE32(counter) ‖ ct ‖ tag`).

use bitchat_core::noise::{PROTOCOL_NAME, StaticIdentity};
use bitchat_core::noise_session::{
    CompletedHandshake, HandshakeInitiator, HandshakeResponder, SessionError, TransportCipher,
};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use snow::resolvers::DefaultResolver;

/// Full hand-rolled initiator ↔ snow responder exchange (snow holds the same
/// static key). Returns my finished handshake and snow's transport state for
/// byte-level cross checks.
fn initiator_vs_snow_responder(init_identity: StaticIdentity) -> (CompletedHandshake, snow::TransportState) {
    let mut snow_hs = snow::Builder::with_resolver(
        PROTOCOL_NAME.parse().unwrap(),
        Box::new(DefaultResolver::default()),
    )
    .local_private_key(&init_identity.private_key)
    .unwrap()
    .build_responder()
    .unwrap();

    let mut init = HandshakeInitiator::new(init_identity);
    let mut buf = vec![0u8; 65535];

    // msg1: mine → snow
    let msg1 = init.step1().unwrap();
    snow_hs.read_message(&msg1, &mut buf).unwrap();

    // msg2: snow → mine
    let msg2_len = snow_hs.write_message(&[], &mut buf).unwrap();
    let msg2 = buf[..msg2_len].to_vec();
    assert_eq!(msg2.len(), 96);
    let (msg3, done) = init.step3(&msg2).unwrap();
    assert_eq!(msg3.len(), 64);

    // msg3: mine → snow, then finalize snow into transport mode.
    snow_hs.read_message(&msg3, &mut buf).unwrap();
    (done, snow_hs.into_transport_mode().unwrap())
}

#[test]
fn xx_handshake_and_keys_match_snow() {
    let identity = StaticIdentity::generate();
    let (mut mine, mut snow_transport) = initiator_vs_snow_responder(identity);

    // My first transport frame (implicit counter 0) must decrypt in snow.
    let frame = mine.send.seal(b"hello snow").unwrap();
    assert_eq!(&frame[..4], &[0, 0, 0, 0], "first send counter is 0");
    let mut buf = vec![0u8; 65535];
    let n = snow_transport.read_message(&frame[4..], &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello snow");

    // And snow's first transport message must decrypt through my receive
    // cipher once its implicit counter is framed as BE32(0).
    let mut out = vec![0u8; 65535];
    let n = snow_transport.write_message(b"hello mine", &mut out).unwrap();
    let mut framed = vec![0, 0, 0, 0];
    framed.extend_from_slice(&out[..n]);
    assert_eq!(mine.recv.open(&framed).unwrap(), b"hello mine");
}

#[test]
fn xx_handshake_full_roundtrip_both_roles() {
    let a = StaticIdentity::generate();
    let b = StaticIdentity::generate();

    let mut init = HandshakeInitiator::new(a.clone());
    let mut resp = HandshakeResponder::new(b.clone());

    let msg1 = init.step1().unwrap();
    let msg2 = resp.step2(&msg1).unwrap();
    let (msg3, mut init_done) = init.step3(&msg2).unwrap();
    let mut resp_done = resp.step4(&msg3).unwrap();

    // Authenticated remote statics cross correctly.
    assert_eq!(init_done.remote_static, b.public_key);
    assert_eq!(resp_done.remote_static, a.public_key);
    assert_eq!(init_done.handshake_hash, resp_done.handshake_hash);

    // Bidirectional transport, several messages, with the extracted-nonce
    // frame on both directions.
    let f1 = init_done.send.seal(b"ping").unwrap();
    assert_eq!(resp_done.recv.open(&f1).unwrap(), b"ping");
    let f2 = resp_done.send.seal(b"pong").unwrap();
    assert_eq!(init_done.recv.open(&f2).unwrap(), b"pong");
    let f3 = init_done.send.seal(b"again").unwrap();
    assert_eq!(resp_done.recv.open(&f3).unwrap(), b"again");
    // The responder's receive counter is independent of its send counter.
    assert_eq!(&f3[..4], &[0, 0, 0, 1]);
}

#[test]
fn tampered_static_key_fails_authentication() {
    let a = StaticIdentity::generate();
    let b = StaticIdentity::generate();

    // Corrupted msg2 must fail on the initiator side.
    let mut init = HandshakeInitiator::new(a.clone());
    let mut resp = HandshakeResponder::new(b.clone());
    let msg1 = init.step1().unwrap();
    let msg2 = resp.step2(&msg1).unwrap();
    let mut msg2_bad = msg2.clone();
    msg2_bad[40] ^= 0x01; // flip a bit inside the encrypted responder static
    assert!(matches!(
        init.step3(&msg2_bad).map(|_| ()),
        Err(SessionError::AuthFailed)
    ));

    // And a corrupted msg3 must fail on the responder side.
    let mut init = HandshakeInitiator::new(a);
    let mut resp = HandshakeResponder::new(b);
    let msg1 = init.step1().unwrap();
    let (mut msg3, _) = init.step3(&resp.step2(&msg1).unwrap()).unwrap();
    msg3[10] ^= 0x01;
    assert!(matches!(
        resp.step4(&msg3).map(|_| ()),
        Err(SessionError::AuthFailed)
    ));
}

#[test]
fn extracted_nonce_frame_matches_manual_construction() {
    // Manual upstream-mirroring construction for key k, counter n:
    // ChaChaPoly key=k, nonce=00000000‖LE64(n), ad empty; frame =
    // BE32(n) ‖ ct ‖ tag.
    let key = [0x42u8; 32];
    let n: u64 = 7;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let mut nonce = [0u8; 12];
    nonce[4..12].copy_from_slice(&n.to_le_bytes());
    let sealed = cipher
        .encrypt(Nonce::from_slice(&nonce), b"frame check".as_slice())
        .unwrap();
    let mut manual = Vec::new();
    manual.extend_from_slice(&(n as u32).to_be_bytes());
    manual.extend_from_slice(&sealed); // ct ‖ tag

    let mut tc = TransportCipher::new(key);
    // The receive path must accept the manual frame regardless of its own
    // send counter state (the nonce is carried in the frame).
    assert_eq!(tc.open(&manual).unwrap(), b"frame check");

    // And our own seal output must round-trip through the manual decoder.
    let frame = tc.seal(b"self seal").unwrap();
    let n_out = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as u64;
    let mut nonce = [0u8; 12];
    nonce[4..12].copy_from_slice(&n_out.to_le_bytes());
    let opened = cipher
        .decrypt(Nonce::from_slice(&nonce), &frame[4..])
        .unwrap();
    assert_eq!(opened, b"self seal");
}

#[test]
fn replay_window_rejects_duplicates_and_old_nonces() {
    let key = [0x11u8; 32];
    let mut tc = TransportCipher::new(key);

    // Encrypt with the underlying key at fixed counters to synthesize
    // out-of-order frames for the receive window.
    let frame_at = |n: u64, pt: &[u8]| {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        let mut nonce = [0u8; 12];
        nonce[4..12].copy_from_slice(&n.to_le_bytes());
        let sealed = cipher.encrypt(Nonce::from_slice(&nonce), pt).unwrap();
        let mut out = (n as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&sealed);
        out
    };

    assert_eq!(tc.open(&frame_at(10, b"a")).unwrap(), b"a");
    // Duplicate → rejected.
    assert!(tc.open(&frame_at(10, b"a")).is_err());
    // Newer accepted; slightly-old (within 1024) accepted; far-old rejected.
    assert_eq!(tc.open(&frame_at(11, b"b")).unwrap(), b"b");
    assert_eq!(tc.open(&frame_at(3, b"c")).unwrap(), b"c");
    assert_eq!(tc.open(&frame_at(1100, b"e")).unwrap(), b"e");
    assert!(tc.open(&frame_at(0, b"d")).is_err()); // > 1024 behind
    // Corrupted tag rejected.
    let mut bad = frame_at(12, b"e");
    let last = bad.len() - 1;
    bad[last] ^= 0xff;
    assert!(tc.open(&bad).is_err());
}
