//! zlib compression policy. Port of `CompressionUtil.swift`.
//!
//! Payloads larger than 256 bytes are compressed when the result is actually
//! smaller; the wire carries the original size in a preamble when compressed.

pub const SHOULD_COMPRESS_THRESHOLD: usize = 256;

pub fn should_compress(payload: &[u8]) -> bool {
    payload.len() > SHOULD_COMPRESS_THRESHOLD
}

pub fn compress(payload: &[u8]) -> Option<Vec<u8>> {
    use flate2::write::ZlibEncoder;
    use std::io::Write;
    let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(payload).ok()?;
    let out = enc.finish().ok()?;
    if out.len() < payload.len() { Some(out) } else { None }
}

pub fn decompress(compressed: &[u8], original_size: usize) -> Option<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    let mut dec = ZlibDecoder::new(compressed);
    let mut out = Vec::with_capacity(original_size);
    dec.read_to_end(&mut out).ok()?;
    if out.len() == original_size { Some(out) } else { None }
}
