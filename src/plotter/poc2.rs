//! PoC2 nonce generation on the CPU, straight from the specification.
//!
//! A nonce is 4096 scoops of 64 bytes.  It is built by seeding a 256 KiB + 16
//! byte buffer with `account_id || nonce` (both big-endian) at the end, then
//! hashing backwards in 32-byte steps: hash `i` covers the bytes from its
//! position up to 4 KiB further on.  The whole buffer is hashed once more and
//! XORed over the data, and PoC2 finally swaps the second half of scoop `s`
//! with the second half of scoop `4095 - s`.
//!
//! This is the reference every faster path (SIMD, GPU) is checked against.

use std::os::raw::c_void;

pub const HASH_SIZE: usize = 32;
pub const SCOOP_SIZE: usize = 64;
pub const SCOOPS_PER_NONCE: usize = 4096;
pub const NONCE_SIZE: usize = SCOOP_SIZE * SCOOPS_PER_NONCE;
pub const HASH_CAP: usize = 4096;
pub const GEN_SIZE: usize = NONCE_SIZE + 16;

extern "C" {
    fn sph_shabal256_init(cc: *mut c_void);
    fn sph_shabal256(cc: *mut c_void, data: *const u8, len: usize);
    fn sph_shabal256_close(cc: *mut c_void, dst: *mut c_void);
}

/// Opaque storage for an `sph_shabal_context` (256 bytes on 64-bit targets).
#[repr(C, align(16))]
struct ShabalContext([u8; 512]);

/// Shabal-256 over the concatenation of `parts`.
pub fn shabal256(parts: &[&[u8]]) -> [u8; HASH_SIZE] {
    let mut ctx = ShabalContext([0u8; 512]);
    let mut out = [0u8; HASH_SIZE];
    // SAFETY: the context buffer is larger than sph_shabal_context and
    // suitably aligned; all pointers are valid for the given lengths.
    unsafe {
        sph_shabal256_init(ctx.0.as_mut_ptr() as *mut c_void);
        for part in parts {
            sph_shabal256(ctx.0.as_mut_ptr() as *mut c_void, part.as_ptr(), part.len());
        }
        sph_shabal256_close(
            ctx.0.as_mut_ptr() as *mut c_void,
            out.as_mut_ptr() as *mut c_void,
        );
    }
    out
}

/// Generate one PoC2 nonce into `out` (exactly `NONCE_SIZE` bytes), using
/// `scratch` (at least `GEN_SIZE` bytes) as working memory.
pub fn generate_nonce_into(account_id: u64, nonce: u64, out: &mut [u8], scratch: &mut [u8]) {
    assert_eq!(out.len(), NONCE_SIZE);
    assert!(scratch.len() >= GEN_SIZE);
    let gendata = &mut scratch[..GEN_SIZE];
    gendata[NONCE_SIZE..NONCE_SIZE + 8].copy_from_slice(&account_id.to_be_bytes());
    gendata[NONCE_SIZE + 8..GEN_SIZE].copy_from_slice(&nonce.to_be_bytes());

    let mut i = NONCE_SIZE;
    while i > 0 {
        let len = (GEN_SIZE - i).min(HASH_CAP);
        let hash = shabal256(&[&gendata[i..i + len]]);
        gendata[i - HASH_SIZE..i].copy_from_slice(&hash);
        i -= HASH_SIZE;
    }

    let final_hash = shabal256(&[&gendata[..]]);
    for (dst, (src, key)) in out
        .iter_mut()
        .zip(gendata[..NONCE_SIZE].iter().zip(final_hash.iter().cycle()))
    {
        *dst = src ^ key;
    }

    poc2_shuffle(out);
}

/// PoC2: swap the second hash of scoop `s` with the second hash of scoop `4095 - s`.
pub fn poc2_shuffle(nonce: &mut [u8]) {
    debug_assert_eq!(nonce.len(), NONCE_SIZE);
    for s in 0..SCOOPS_PER_NONCE / 2 {
        let a = s * SCOOP_SIZE + HASH_SIZE;
        let b = (SCOOPS_PER_NONCE - 1 - s) * SCOOP_SIZE + HASH_SIZE;
        let (lo, hi) = nonce.split_at_mut(b);
        lo[a..a + HASH_SIZE].swap_with_slice(&mut hi[..HASH_SIZE]);
    }
}

/// Convenience wrapper that allocates.
#[allow(dead_code)]
pub fn generate_nonce(account_id: u64, nonce: u64) -> Vec<u8> {
    let mut out = vec![0u8; NONCE_SIZE];
    let mut scratch = vec![0u8; GEN_SIZE];
    generate_nonce_into(account_id, nonce, &mut out, &mut scratch);
    out
}

/// Fill `batch` (nonce-major, `count * NONCE_SIZE` bytes) with nonces
/// `start_nonce..start_nonce + count` using all available cores.
pub fn generate_batch(account_id: u64, start_nonce: u64, batch: &mut [u8]) {
    use rayon::prelude::*;
    batch.par_chunks_mut(NONCE_SIZE).enumerate().for_each_init(
        || vec![0u8; GEN_SIZE],
        |scratch, (i, out)| {
            generate_nonce_into(account_id, start_nonce + i as u64, out, scratch);
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn shabal256_matches_known_vector() {
        // Shabal-256 of the empty message, from the SHA-3 submission test vectors.
        let empty = shabal256(&[]);
        assert_eq!(
            hex::encode(empty),
            "aec750d11feee9f16271922fbaf5a9be142f62019ef8d720f858940070889014"
        );
    }

    /// The repository's test plot was made by a real plotter; our generator
    /// must reproduce it (its final 8 bytes were zero-filled by that plotter).
    #[test]
    fn generated_nonces_match_real_plot_file() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_data")
            .join("10282355196851764065_0_8");
        let on_disk = std::fs::read(path).expect("test plot present");
        let mut batch = vec![0u8; 8 * NONCE_SIZE];
        generate_batch(10282355196851764065, 0, &mut batch);
        for n in 0..8usize {
            for s in 0..SCOOPS_PER_NONCE {
                let disk = &on_disk[(s * 8 + n) * SCOOP_SIZE..(s * 8 + n + 1) * SCOOP_SIZE];
                let ours =
                    &batch[n * NONCE_SIZE + s * SCOOP_SIZE..n * NONCE_SIZE + (s + 1) * SCOOP_SIZE];
                if n == 7 && s == SCOOPS_PER_NONCE - 1 {
                    assert_eq!(&disk[..56], &ours[..56]);
                    assert_eq!(&disk[56..], &[0u8; 8]);
                } else {
                    assert_eq!(disk, ours, "nonce {} scoop {}", n, s);
                }
            }
        }
    }
}
