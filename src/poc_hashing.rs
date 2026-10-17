use crate::shabal256::{shabal256_deadline_fast, shabal256_hash_fast};

#[cfg_attr(not(any(test, simd_backend = "none")), allow(dead_code))]
const SCOOP_SIZE: usize = 64;

/// Decode a generation signature as sent by the pool/wallet.
/// Returns `None` when it is not exactly 32 bytes of hex.
pub fn decode_gensig(gensig: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(gensig.trim()).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut gensig_bytes = [0; 32];
    gensig_bytes.copy_from_slice(&bytes);
    Some(gensig_bytes)
}

pub fn calculate_scoop(height: u64, gensig: &[u8; 32]) -> u32 {
    let mut data: [u8; 64] = [0; 64];
    let height_bytes: [u8; 8] = height.to_be_bytes();

    data[..32].clone_from_slice(gensig);
    data[32..40].clone_from_slice(&height_bytes);
    data[40] = 0x80;
    let mut words = [0u32; 16];
    for (w, chunk) in words.iter_mut().zip(data.chunks_exact(4)) {
        *w = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }

    let new_gensig = &shabal256_hash_fast(&[], &words);
    (u32::from(new_gensig[30] & 0x0F) << 8) | u32::from(new_gensig[31])
}

/// Pure Rust implementation for finding the best deadline (fallback/reference).
#[cfg_attr(not(any(test, simd_backend = "none")), allow(dead_code))]
pub fn find_best_deadline_rust(
    data: &[u8],
    number_of_nonces: u64,
    gensig: &[u8; 32],
) -> (u64, u64) {
    let mut best_deadline = u64::MAX;
    let mut best_offset = 0;
    for i in 0..number_of_nonces as usize {
        let result =
            shabal256_deadline_fast(&data[i * SCOOP_SIZE..i * SCOOP_SIZE + SCOOP_SIZE], gensig);
        if result < best_deadline {
            best_deadline = result;
            best_offset = i;
        }
    }
    (best_deadline, best_offset as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_gensig_validates_input() {
        assert!(decode_gensig("zz").is_none());
        assert!(decode_gensig("00").is_none());
        let ok = decode_gensig("4a6f686e6e7946464d206861742064656e206772f6df74656e2050656e697321")
            .unwrap();
        assert_eq!(ok[0], 0x4a);
        assert_eq!(ok[31], 0x21);
    }

    #[test]
    fn scoop_is_in_range_and_deterministic() {
        let gensig =
            decode_gensig("4a6f686e6e7946464d206861742064656e206772f6df74656e2050656e697321")
                .unwrap();
        let a = calculate_scoop(1_000_000, &gensig);
        let b = calculate_scoop(1_000_000, &gensig);
        assert_eq!(a, b);
        assert!(a < 4096);
        assert_ne!(a, calculate_scoop(1_000_001, &gensig));
    }

    /// Cross-check the scoop derivation against the independent Shabal
    /// implementation: scoop = last 12 bits of Shabal-256(gensig || height BE).
    #[test]
    fn scoop_matches_independent_shabal() {
        let gensig =
            decode_gensig("4a6f686e6e7946464d206861742064656e206772f6df74656e2050656e697321")
                .unwrap();
        for height in [1u64, 12345, 1_234_567] {
            let hash = crate::test_support::shabal256(&[&gensig, &height.to_be_bytes()]);
            let expected = (u32::from(hash[30] & 0x0F) << 8) | u32::from(hash[31]);
            assert_eq!(calculate_scoop(height, &gensig), expected);
        }
    }
}
