/*
 * PoC2 nonce generation, LANES nonces at a time.
 *
 * The whole cost of plotting is Shabal-256: a nonce is 8192 chained hashes
 * over a 256 KiB buffer plus one hash over the whole buffer, about 530 000
 * Shabal block compressions.  The parallel Shabal implementations in this
 * directory (SSE2/AVX/NEON: 4 lanes, AVX2: 8, AVX-512: 16) compress one
 * 64-byte block for every lane at once, so this file keeps the working data of
 * LANES nonces word-interleaved - 32-bit word w of nonce k sits at
 * gen[w * LANES + k] - and the whole chain is hashed for all lanes together.
 *
 * Input lengths of the chained hashes are 32*h + 16 bytes, capped at 4096,
 * so after the full 64-byte blocks there are 16, 48 or 0 bytes left; they are
 * copied into a padding block ("termination", 0x80 after the data, zeros to
 * the end) that `mshabal_hash_fast_*` consumes as the last block.
 *
 * Output is written straight into a scoop-major chunk of `chunk_nonces`
 * nonces: scoop s of nonce index n is at out[(s * chunk_nonces + n) * 64],
 * with the final XOR and the PoC2 swap applied on the way out.
 *
 * Compiled without a NONCEGEN_* define this is the scalar reference
 * (LANES = 1, sph_shabal); the build script compiles it a second time with
 * the backend it picked for the miner.
 */

#include <stdint.h>
#include <string.h>

#include "common.h"
#include "noncegen.h"
#include "sph_shabal.h"

#if defined(NONCEGEN_AVX512F)
#include "mshabal_512_avx512f.h"
#define LANES 16
#define NC_NAME noncegen_simd
typedef mshabal512_context nc_context;
typedef mshabal512_context_fast nc_context_fast;
#define NC_INIT mshabal_init_avx512f
#define NC_HASH mshabal_hash_fast_avx512f
#elif defined(NONCEGEN_AVX2)
#include "mshabal_256_avx2.h"
#define LANES 8
#define NC_NAME noncegen_simd
typedef mshabal256_context nc_context;
typedef mshabal256_context_fast nc_context_fast;
#define NC_INIT mshabal_init_avx2
#define NC_HASH mshabal_hash_fast_avx2
#elif defined(NONCEGEN_AVX)
#include "mshabal_128_avx.h"
#define LANES 4
#define NC_NAME noncegen_simd
typedef mshabal128_context nc_context;
typedef mshabal128_context_fast nc_context_fast;
#define NC_INIT mshabal_init_avx
#define NC_HASH mshabal_hash_fast_avx
#elif defined(NONCEGEN_SSE2)
#include "mshabal_128_sse2.h"
#define LANES 4
#define NC_NAME noncegen_simd
typedef mshabal128_context nc_context;
typedef mshabal128_context_fast nc_context_fast;
#define NC_INIT mshabal_init_sse2
#define NC_HASH mshabal_hash_fast_sse2
#elif defined(NONCEGEN_NEON)
#include "mshabal_128_neon.h"
#define LANES 4
#define NC_NAME noncegen_simd
typedef mshabal128_context nc_context;
typedef mshabal128_context_fast nc_context_fast;
#define NC_INIT mshabal_init_neon
#define NC_HASH mshabal_hash_fast_neon
#else
#define LANES 1
#define NC_NAME noncegen_scalar
#define NC_SCALAR 1
#endif

/* On x86 the AVX2 and AVX-512 variants are always built alongside the
   selected backend and chosen at run time; they get their own names. */
#ifdef NONCEGEN_NAME
#undef NC_NAME
#define NC_NAME NONCEGEN_NAME
#endif

#define NC_CAT2(a, b) a##b
#define NC_CAT(a, b) NC_CAT2(a, b)
#define NC_FN(suffix) NC_CAT(NC_NAME, suffix)

#define NONCE_WORDS (NONCE_SIZE / 4)
#define GEN_WORDS (NONCE_WORDS + 4)
#define BLOCK_WORDS 16
#define HASH_WORDS (HASH_SIZE / 4)

#ifdef NC_SCALAR
typedef struct {
    int unused;
} nc_context_fast;

static void nc_hash(nc_context_fast *ctx, const uint32_t *message, const uint32_t *termination,
                    uint32_t *dst, unsigned num_blocks) {
    (void)ctx;
    sph_shabal_hash_fast((void *)message, (void *)termination, dst, num_blocks);
}
#else
static nc_context_fast global_fast;

static void nc_hash(nc_context_fast *ctx, const uint32_t *message, const uint32_t *termination,
                    uint32_t *dst, unsigned num_blocks) {
    NC_HASH(ctx, (void *)message, (void *)termination, dst, num_blocks);
}
#endif

void NC_FN(_init)(void) {
#ifndef NC_SCALAR
    nc_context full;
    NC_INIT(&full, 256);
    global_fast.out_size = full.out_size;
    memcpy(global_fast.state, full.state, sizeof(global_fast.state));
    global_fast.Whigh = full.Whigh;
    global_fast.Wlow = full.Wlow;
#endif
}

unsigned NC_FN(_lanes)(void) { return LANES; }

/* gen + three termination blocks + the final hash, all LANES wide. */
size_t NC_FN(_scratch_words)(void) { return (size_t)(GEN_WORDS + 4 * BLOCK_WORDS) * LANES; }

/*
 * Generate nonces start_nonce .. start_nonce + LANES - 1 and write the first
 * `count` of them (count <= LANES) as nonce indexes offset .. offset + count - 1
 * of the scoop-major chunk `out` that holds `chunk_nonces` nonces.
 *
 * `scratch` must hold `*_scratch_words()` 32-bit words and be 64-byte aligned.
 */
void NC_NAME(uint64_t account_id, uint64_t start_nonce, uint32_t *scratch, uint8_t *out,
             size_t chunk_nonces, size_t offset, size_t count) {
    uint32_t *gen = scratch;
    uint32_t *term0 = gen + (size_t)GEN_WORDS * LANES; /* padding only (4096-byte inputs) */
    uint32_t *term16 = term0 + BLOCK_WORDS * LANES;    /* 16 data bytes + padding */
    uint32_t *term48 = term16 + BLOCK_WORDS * LANES;   /* 48 data bytes + padding */
    uint32_t *fin = term48 + BLOCK_WORDS * LANES;
    nc_context_fast ctx;
    size_t i, k, s;

#ifdef NC_SCALAR
    memset(&ctx, 0, sizeof(ctx));
#else
    memcpy(&ctx, &global_fast, sizeof(ctx));
#endif

    memset(term0, 0, 3 * BLOCK_WORDS * LANES * sizeof(uint32_t));
    for (k = 0; k < LANES; k++) {
        term0[k] = 0x80;
        term16[4 * LANES + k] = 0x80;
        term48[12 * LANES + k] = 0x80;
    }

    /* Seed: account id || nonce, big-endian, in the last 16 bytes of every lane. */
    {
        uint32_t a_hi = bswap_32((uint32_t)(account_id >> 32));
        uint32_t a_lo = bswap_32((uint32_t)(account_id & 0xFFFFFFFFu));
        for (k = 0; k < LANES; k++) {
            uint64_t nonce = start_nonce + (uint64_t)k;
            gen[(NONCE_WORDS + 0) * LANES + k] = a_hi;
            gen[(NONCE_WORDS + 1) * LANES + k] = a_lo;
            gen[(NONCE_WORDS + 2) * LANES + k] = bswap_32((uint32_t)(nonce >> 32));
            gen[(NONCE_WORDS + 3) * LANES + k] = bswap_32((uint32_t)(nonce & 0xFFFFFFFFu));
        }
    }

    /* Chain backwards: hash i covers [i, i + min(GEN_SIZE - i, 4096)). */
    for (i = NONCE_SIZE; i > 0; i -= HASH_SIZE) {
        size_t len = NONCE_SIZE + 16 - i;
        size_t full, tail_words;
        uint32_t *msg, *term;
        if (len > HASH_CAP) len = HASH_CAP;
        full = len / 64;
        tail_words = (len % 64) / 4; /* 4, 12 or 0 */
        msg = gen + (i / 4) * LANES;
        term = tail_words == 4 ? term16 : (tail_words == 12 ? term48 : term0);
        memcpy(term, msg + full * BLOCK_WORDS * LANES, tail_words * LANES * sizeof(uint32_t));
        nc_hash(&ctx, msg, term, gen + ((i - HASH_SIZE) / 4) * LANES, (unsigned)full);
    }

    /* Final hash over the whole buffer (4096 full blocks + the 16-byte seed). */
    memcpy(term16, gen + (size_t)NONCE_WORDS * LANES, 4 * LANES * sizeof(uint32_t));
    nc_hash(&ctx, gen, term16, fin, NONCE_WORDS / BLOCK_WORDS);

    /* XOR every word with the matching word of the final hash. */
    for (i = 0; i < NONCE_WORDS; i += HASH_WORDS) {
        uint32_t *row = gen + i * LANES;
        size_t j;
        for (j = 0; j < HASH_WORDS * LANES; j++) {
            row[j] ^= fin[j];
        }
    }

    /* De-interleave into the scoop-major chunk with the PoC2 swap:
       scoop s = first hash of scoop s, second hash of scoop 4095 - s. */
    for (s = 0; s < NUM_SCOOPS; s++) {
        const uint32_t *h0 = gen + (s * 16) * LANES;
        const uint32_t *h1 = gen + ((NUM_SCOOPS - 1 - s) * 16 + HASH_WORDS) * LANES;
        uint8_t *row = out + (s * chunk_nonces + offset) * SCOOP_SIZE;
        for (k = 0; k < count; k++) {
            uint32_t *dst = (uint32_t *)(row + k * SCOOP_SIZE);
            size_t j;
            for (j = 0; j < HASH_WORDS; j++) {
                dst[j] = h0[j * LANES + k];
                dst[HASH_WORDS + j] = h1[j * LANES + k];
            }
        }
    }
}
