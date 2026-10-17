//! Test-only helpers: an independent PoC2 plot generator and deadline oracle.
//!
//! Everything in here is derived from the PoC2 specification rather than from
//! the miner's own read path, so it can be used to verify that the miner maps
//! bytes on disk to the correct nonce numbers and deadlines.

#[path = "plotter/poc2.rs"]
#[allow(dead_code)]
mod poc2;

pub use poc2::{generate_nonce, shabal256};
use poc2::{NONCE_SIZE, SCOOPS_PER_NONCE, SCOOP_SIZE};

/// The 64-byte scoop `scoop` of a generated nonce.
pub fn scoop_of(nonce_data: &[u8], scoop: u32) -> &[u8] {
    let start = scoop as usize * SCOOP_SIZE;
    &nonce_data[start..start + SCOOP_SIZE]
}

/// Build an "optimized" (scoop-major) PoC2 plot file image for the given nonces.
pub fn generate_plot_image(account_id: u64, start_nonce: u64, nonces: u64) -> Vec<u8> {
    let generated: Vec<Vec<u8>> = (0..nonces)
        .map(|n| generate_nonce(account_id, start_nonce + n))
        .collect();
    let mut image = vec![0u8; nonces as usize * NONCE_SIZE];
    for scoop in 0..SCOOPS_PER_NONCE {
        for (n, data) in generated.iter().enumerate() {
            let dst = (scoop * nonces as usize + n) * SCOOP_SIZE;
            image[dst..dst + SCOOP_SIZE].copy_from_slice(scoop_of(data, scoop as u32));
        }
    }
    image
}

/// Unadjusted deadline for a scoop: Shabal-256(gensig || scoop), first 8 bytes LE.
pub fn deadline_for_scoop(gensig: &[u8; 32], scoop_data: &[u8]) -> u64 {
    let hash = shabal256(&[gensig, scoop_data]);
    u64::from_le_bytes(hash[..8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shabal256_matches_known_vector() {
        // Shabal-256 of the empty message, from the SHA-3 submission test vectors.
        let empty = shabal256(&[]);
        assert_eq!(
            hex::encode(empty),
            "aec750d11feee9f16271922fbaf5a9be142f62019ef8d720f858940070889014"
        );
    }

    #[test]
    fn deadline_oracle_matches_miner_reference_hasher() {
        // Same vector as cpu_worker::tests::test_deadline_hashing.
        let gensig_vec =
            hex::decode("4a6f686e6e7946464d206861742064656e206772f6df74656e2050656e697321")
                .unwrap();
        let mut gensig = [0u8; 32];
        gensig.copy_from_slice(&gensig_vec);
        assert_eq!(
            deadline_for_scoop(&gensig, &[0u8; 64]),
            3084580316385335914u64
        );
        assert_eq!(
            deadline_for_scoop(&gensig, &[0u8; 64]),
            crate::shabal256::shabal256_deadline_fast(&[0u8; 64], &gensig)
        );
    }

    #[test]
    fn generated_plot_matches_real_plot_file() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_data")
            .join("10282355196851764065_0_8");
        let on_disk = std::fs::read(path).expect("test plot present");
        let image = generate_plot_image(10282355196851764065, 0, 8);
        assert_eq!(on_disk.len(), image.len());
        // The plotter that produced the test file zero-filled the final 8 bytes
        // of the last scoop; everything else must match exactly.
        let compare_len = on_disk.len() - 8;
        if on_disk[..compare_len] != image[..compare_len] {
            let first = on_disk
                .iter()
                .zip(image.iter())
                .position(|(a, b)| a != b)
                .unwrap();
            panic!(
                "generated PoC2 plot differs from the real plot file: first diff at byte {} (scoop {}, nonce {})",
                first,
                first / (8 * 64),
                (first / 64) % 8
            );
        }
        assert_eq!(&on_disk[compare_len..], &[0u8; 8]);
    }
}
