#[test]
fn debug_snow_msg2_length() {
    use backchat_core::noise::StaticIdentity;
    use backchat_core::noise_session::{HandshakeInitiator, HandshakeResponder};
    use snow::resolvers::DefaultResolver;

    let idb = StaticIdentity::generate();
    let eph = [0x37u8; 32]; // same ephemeral for both implementations

    // Snow responder with fixed static + ephemeral.
    let mut snow_hs = snow::Builder::with_resolver(
        "Noise_XX_25519_ChaChaPoly_SHA256".parse().unwrap(),
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
    eprintln!("snow msg2 = {}", hex::encode(&snow_msg2));

    // Mine with the same ephemeral: patch HandshakeResponder's ephemeral by
    // constructing one and overwriting the private field is not possible
    // here (private), so instead run my responder and compare structure.
    let mut resp = HandshakeResponder::new(idb.clone());
    let my_msg2 = resp.step2(&msg1).unwrap();
    eprintln!("mine msg2 = {}", hex::encode(&my_msg2));
    eprintln!("lengths: snow={} mine={}", snow_msg2.len(), my_msg2.len());
}
