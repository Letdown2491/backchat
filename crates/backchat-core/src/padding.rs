//! PKCS#7-style bucket padding for Noise frames.
//! Port of `BitFoundation/MessagePadding.swift`.

pub const BLOCK_SIZES: [usize; 4] = [256, 512, 1024, 2048];

/// Pad `data` up to `target_size` with PKCS#7-style padding (every pad byte
/// equals the pad length). Declines when more than 255 bytes would be needed
/// — a frame far below its bucket is emitted unpadded, by upstream rule.
pub fn pad(data: &[u8], target_size: usize) -> Vec<u8> {
    let mut out = data.to_vec();
    let padding_needed = target_size.saturating_sub(out.len());
    if out.len() < target_size && padding_needed > 0 && padding_needed <= 255 {
        out.extend(std::iter::repeat_n(padding_needed as u8, padding_needed));
    }
    out
}

/// Strip valid PKCS#7 padding; returns `data` unchanged when the tail is not
/// valid padding (upstream behavior: never fails, just gives up).
pub fn unpad(data: &[u8]) -> &[u8] {
    let Some(&last) = data.last() else { return data };
    let padding_length = last as usize;
    if padding_length == 0 || padding_length > data.len() {
        return data;
    }
    let tail = &data[data.len() - padding_length..];
    if tail.iter().any(|&b| b != last) {
        return data;
    }
    &data[..data.len() - padding_length]
}

/// Smallest bucket that fits `data_size`, accounting for the 16-byte AEAD tag
/// overhead exactly as upstream does. Oversized frames are returned as-is
/// (they get fragmented anyway).
pub fn optimal_block_size(data_size: usize) -> usize {
    let total = data_size + 16;
    for &block in &BLOCK_SIZES {
        if total <= block {
            return block;
        }
    }
    data_size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_within_255() {
        let data = [0u8; 100];
        let target = optimal_block_size(100); // 100 + 16 -> 256
        let padded = pad(&data, target);
        assert_eq!(padded.len(), 256);
        assert_eq!(unpad(&padded), &data[..]);
    }

    #[test]
    fn too_far_from_bucket_stays_unpadded() {
        let data = [0u8; 100];
        // Needs 1556 pad bytes to reach 2048 -> declined upstream rule
        let padded = pad(&data, 2048);
        assert_eq!(padded.len(), 100);
    }
}
