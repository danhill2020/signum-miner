#pragma once

#include <stddef.h>
#include <stdint.h>

/*
 * PoC2 nonce generation (see noncegen.c).  The same source is compiled twice:
 * once as the scalar reference (`noncegen_scalar*`) and once with the SIMD
 * backend the build selected (`noncegen_simd*`, LANES nonces at a time).
 */

#define NONCEGEN_DECL(prefix)                                                         \
    void prefix##_init(void);                                                         \
    unsigned prefix##_lanes(void);                                                    \
    size_t prefix##_scratch_words(void);                                              \
    void prefix(uint64_t account_id, uint64_t start_nonce, uint32_t *scratch,        \
                uint8_t *out, size_t chunk_nonces, size_t offset, size_t count);

NONCEGEN_DECL(noncegen_scalar)
NONCEGEN_DECL(noncegen_simd)
/* x86 only: always built, used when the CPU supports them. */
NONCEGEN_DECL(noncegen_avx2)
NONCEGEN_DECL(noncegen_avx512f)
