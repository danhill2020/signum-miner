/*
 * PoC2 nonce generation on the GPU.
 *
 * One work-item generates one nonce:
 *
 *   gendata[NONCE_SIZE..] = account_id (BE) || nonce (BE)
 *   for i = NONCE_SIZE down to 32 step 32:
 *       gendata[i-32..i] = shabal256(gendata[i .. i + min(GEN_SIZE - i, 4096)])
 *   final = shabal256(gendata[0..GEN_SIZE])
 *   scoop s = (gendata[s*64 .. +32], gendata[(4095-s)*64 + 32 .. +32]) ^ final
 *
 * Memory layout is what makes this fast: the work buffer is interleaved by
 * nonce, 32-bit word w of work-item g sits at gendata[w * n + g] (n = padded
 * batch size).  Neighbouring work-items therefore read and write neighbouring
 * words and every access of a wavefront is one coalesced transaction; a
 * nonce-per-slice layout would touch a different cache line per work-item.
 * Chaining 8192 hashes is ~34 MB of reads per nonce, so this is the
 * difference between a memory-bound and a compute-bound kernel.
 *
 * `generate_nonces` runs the chain and stores the final hash;
 * `finish_nonces` (one work-item per scoop and nonce) applies the XOR and the
 * PoC2 swap and writes scoop-major output, `out[(s * count + g) * 64]`, which
 * the host copies straight into its write chunk.
 *
 * Every hashed length is a multiple of 4 bytes, so the message is handled as
 * 32-bit words throughout and the 0x80 padding byte always starts a fresh
 * word.  The Shabal round functions are the reference ones (SAPHIR project).
 */

typedef unsigned int sph_u32;

#define SPH_C32(x)    ((sph_u32)(x ## U))
#define SPH_T32(x)    (as_uint(x))
#define C32   SPH_C32
#define T32   SPH_T32
#define O1   13
#define O2    9
#define O3    6

#define INPUT_BLOCK_ADD   do { \
		B0 = T32(B0 + M0); B1 = T32(B1 + M1); B2 = T32(B2 + M2); B3 = T32(B3 + M3); \
		B4 = T32(B4 + M4); B5 = T32(B5 + M5); B6 = T32(B6 + M6); B7 = T32(B7 + M7); \
		B8 = T32(B8 + M8); B9 = T32(B9 + M9); BA = T32(BA + MA); BB = T32(BB + MB); \
		BC = T32(BC + MC); BD = T32(BD + MD); BE = T32(BE + ME); BF = T32(BF + MF); \
	} while (0)

#define INPUT_BLOCK_SUB   do { \
		C0 = T32(C0 - M0); C1 = T32(C1 - M1); C2 = T32(C2 - M2); C3 = T32(C3 - M3); \
		C4 = T32(C4 - M4); C5 = T32(C5 - M5); C6 = T32(C6 - M6); C7 = T32(C7 - M7); \
		C8 = T32(C8 - M8); C9 = T32(C9 - M9); CA = T32(CA - MA); CB = T32(CB - MB); \
		CC = T32(CC - MC); CD = T32(CD - MD); CE = T32(CE - ME); CF = T32(CF - MF); \
	} while (0)

#define XOR_W   do { A00 ^= Wlow; A01 ^= Whigh; } while (0)

#define SWAP(v1, v2)   do { sph_u32 tmp = (v1); (v1) = (v2); (v2) = tmp; } while (0)

#define SWAP_BC   do { \
		SWAP(B0, C0); SWAP(B1, C1); SWAP(B2, C2); SWAP(B3, C3); \
		SWAP(B4, C4); SWAP(B5, C5); SWAP(B6, C6); SWAP(B7, C7); \
		SWAP(B8, C8); SWAP(B9, C9); SWAP(BA, CA); SWAP(BB, CB); \
		SWAP(BC, CC); SWAP(BD, CD); SWAP(BE, CE); SWAP(BF, CF); \
	} while (0)

#define PERM_ELT(xa0, xa1, xb0, xb1, xb2, xb3, xc, xm)   do { \
		xa0 = T32((xa0 \
			^ (T32(rotate(xa1, 15U) * 5U)) \
			^ xc) * 3U) \
			^ xb1 ^ (xb2 & ~xb3) ^ xm; \
		xb0 = T32(~(T32(rotate(xb0, 1U)) ^ xa0)); \
	} while (0)

#define PERM_STEP_0   do { \
		PERM_ELT(A00, A0B, B0, BD, B9, B6, C8, M0); \
		PERM_ELT(A01, A00, B1, BE, BA, B7, C7, M1); \
		PERM_ELT(A02, A01, B2, BF, BB, B8, C6, M2); \
		PERM_ELT(A03, A02, B3, B0, BC, B9, C5, M3); \
		PERM_ELT(A04, A03, B4, B1, BD, BA, C4, M4); \
		PERM_ELT(A05, A04, B5, B2, BE, BB, C3, M5); \
		PERM_ELT(A06, A05, B6, B3, BF, BC, C2, M6); \
		PERM_ELT(A07, A06, B7, B4, B0, BD, C1, M7); \
		PERM_ELT(A08, A07, B8, B5, B1, BE, C0, M8); \
		PERM_ELT(A09, A08, B9, B6, B2, BF, CF, M9); \
		PERM_ELT(A0A, A09, BA, B7, B3, B0, CE, MA); \
		PERM_ELT(A0B, A0A, BB, B8, B4, B1, CD, MB); \
		PERM_ELT(A00, A0B, BC, B9, B5, B2, CC, MC); \
		PERM_ELT(A01, A00, BD, BA, B6, B3, CB, MD); \
		PERM_ELT(A02, A01, BE, BB, B7, B4, CA, ME); \
		PERM_ELT(A03, A02, BF, BC, B8, B5, C9, MF); \
	} while (0)

#define PERM_STEP_1   do { \
		PERM_ELT(A04, A03, B0, BD, B9, B6, C8, M0); \
		PERM_ELT(A05, A04, B1, BE, BA, B7, C7, M1); \
		PERM_ELT(A06, A05, B2, BF, BB, B8, C6, M2); \
		PERM_ELT(A07, A06, B3, B0, BC, B9, C5, M3); \
		PERM_ELT(A08, A07, B4, B1, BD, BA, C4, M4); \
		PERM_ELT(A09, A08, B5, B2, BE, BB, C3, M5); \
		PERM_ELT(A0A, A09, B6, B3, BF, BC, C2, M6); \
		PERM_ELT(A0B, A0A, B7, B4, B0, BD, C1, M7); \
		PERM_ELT(A00, A0B, B8, B5, B1, BE, C0, M8); \
		PERM_ELT(A01, A00, B9, B6, B2, BF, CF, M9); \
		PERM_ELT(A02, A01, BA, B7, B3, B0, CE, MA); \
		PERM_ELT(A03, A02, BB, B8, B4, B1, CD, MB); \
		PERM_ELT(A04, A03, BC, B9, B5, B2, CC, MC); \
		PERM_ELT(A05, A04, BD, BA, B6, B3, CB, MD); \
		PERM_ELT(A06, A05, BE, BB, B7, B4, CA, ME); \
		PERM_ELT(A07, A06, BF, BC, B8, B5, C9, MF); \
	} while (0)

#define PERM_STEP_2   do { \
		PERM_ELT(A08, A07, B0, BD, B9, B6, C8, M0); \
		PERM_ELT(A09, A08, B1, BE, BA, B7, C7, M1); \
		PERM_ELT(A0A, A09, B2, BF, BB, B8, C6, M2); \
		PERM_ELT(A0B, A0A, B3, B0, BC, B9, C5, M3); \
		PERM_ELT(A00, A0B, B4, B1, BD, BA, C4, M4); \
		PERM_ELT(A01, A00, B5, B2, BE, BB, C3, M5); \
		PERM_ELT(A02, A01, B6, B3, BF, BC, C2, M6); \
		PERM_ELT(A03, A02, B7, B4, B0, BD, C1, M7); \
		PERM_ELT(A04, A03, B8, B5, B1, BE, C0, M8); \
		PERM_ELT(A05, A04, B9, B6, B2, BF, CF, M9); \
		PERM_ELT(A06, A05, BA, B7, B3, B0, CE, MA); \
		PERM_ELT(A07, A06, BB, B8, B4, B1, CD, MB); \
		PERM_ELT(A08, A07, BC, B9, B5, B2, CC, MC); \
		PERM_ELT(A09, A08, BD, BA, B6, B3, CB, MD); \
		PERM_ELT(A0A, A09, BE, BB, B7, B4, CA, ME); \
		PERM_ELT(A0B, A0A, BF, BC, B8, B5, C9, MF); \
	} while (0)

#define APPLY_P   do { \
		B0 = T32(rotate(B0, 17U)); B1 = T32(rotate(B1, 17U)); B2 = T32(rotate(B2, 17U)); B3 = T32(rotate(B3, 17U)); \
		B4 = T32(rotate(B4, 17U)); B5 = T32(rotate(B5, 17U)); B6 = T32(rotate(B6, 17U)); B7 = T32(rotate(B7, 17U)); \
		B8 = T32(rotate(B8, 17U)); B9 = T32(rotate(B9, 17U)); BA = T32(rotate(BA, 17U)); BB = T32(rotate(BB, 17U)); \
		BC = T32(rotate(BC, 17U)); BD = T32(rotate(BD, 17U)); BE = T32(rotate(BE, 17U)); BF = T32(rotate(BF, 17U)); \
		PERM_STEP_0; \
		PERM_STEP_1; \
		PERM_STEP_2; \
		A0B = T32(A0B + C6); A0A = T32(A0A + C5); A09 = T32(A09 + C4); A08 = T32(A08 + C3); \
		A07 = T32(A07 + C2); A06 = T32(A06 + C1); A05 = T32(A05 + C0); A04 = T32(A04 + CF); \
		A03 = T32(A03 + CE); A02 = T32(A02 + CD); A01 = T32(A01 + CC); A00 = T32(A00 + CB); \
		A0B = T32(A0B + CA); A0A = T32(A0A + C9); A09 = T32(A09 + C8); A08 = T32(A08 + C7); \
		A07 = T32(A07 + C6); A06 = T32(A06 + C5); A05 = T32(A05 + C4); A04 = T32(A04 + C3); \
		A03 = T32(A03 + C2); A02 = T32(A02 + C1); A01 = T32(A01 + C0); A00 = T32(A00 + CF); \
		A0B = T32(A0B + CE); A0A = T32(A0A + CD); A09 = T32(A09 + CC); A08 = T32(A08 + CB); \
		A07 = T32(A07 + CA); A06 = T32(A06 + C9); A05 = T32(A05 + C8); A04 = T32(A04 + C7); \
		A03 = T32(A03 + C6); A02 = T32(A02 + C5); A01 = T32(A01 + C4); A00 = T32(A00 + C3); \
	} while (0)

#define INCR_W   do { if ((Wlow = T32(Wlow + 1)) == 0) Whigh = T32(Whigh + 1); } while (0)

__constant static const sph_u32 A_init_256[] = {
    C32(0x52F84552), C32(0xE54B7999), C32(0x2D8EE3EC), C32(0xB9645191),
    C32(0xE0078B86), C32(0xBB7C44C9), C32(0xD2B5C1CA), C32(0xB0D2EB8C),
    C32(0x14CE5A45), C32(0x22AF50DC), C32(0xEFFDBC6B), C32(0xEB21B74A)
};
__constant static const sph_u32 B_init_256[] = {
    C32(0xB555C6EE), C32(0x3E710596), C32(0xA72A652F), C32(0x9301515F),
    C32(0xDA28C1FA), C32(0x696FD868), C32(0x9CB6BF72), C32(0x0AFE4002),
    C32(0xA6E03615), C32(0x5138C1D4), C32(0xBE216306), C32(0xB38B8890),
    C32(0x3EA8B96B), C32(0x3299ACE4), C32(0x30924DD4), C32(0x55CB34A5)
};
__constant static const sph_u32 C_init_256[] = {
    C32(0xB405F031), C32(0xC4233EBA), C32(0xB3733979), C32(0xC0DD9D55),
    C32(0xC51C28AE), C32(0xA327B8E1), C32(0x56C56167), C32(0xED614433),
    C32(0x88B59D60), C32(0x60E2CEBA), C32(0x758B4B8B), C32(0x83E82A7F),
    C32(0xBC968828), C32(0xE6E00BF7), C32(0xBA839E55), C32(0x9B491C60)
};

#define HASH_SIZE        32
#define SCOOP_SIZE       64
#define SCOOPS_PER_NONCE 4096
#define NONCE_SIZE       (SCOOP_SIZE * SCOOPS_PER_NONCE)
#define HASH_CAP         4096
#define GEN_SIZE         (NONCE_SIZE + 16)
#define GEN_WORDS        (GEN_SIZE / 4)
#define NONCE_WORDS      (NONCE_SIZE / 4)

/*
 * Shabal-256 of `len_words` 32-bit words, word k of the message at
 * msg[k * stride].  Writes 8 words to `out`.
 */
static void shabal256_words(__global const sph_u32* msg, uint stride, uint len_words, sph_u32* out)
{
    sph_u32
        A00 = A_init_256[0], A01 = A_init_256[1], A02 = A_init_256[2], A03 = A_init_256[3],
        A04 = A_init_256[4], A05 = A_init_256[5], A06 = A_init_256[6], A07 = A_init_256[7],
        A08 = A_init_256[8], A09 = A_init_256[9], A0A = A_init_256[10], A0B = A_init_256[11];
    sph_u32
        B0 = B_init_256[0], B1 = B_init_256[1], B2 = B_init_256[2], B3 = B_init_256[3],
        B4 = B_init_256[4], B5 = B_init_256[5], B6 = B_init_256[6], B7 = B_init_256[7],
        B8 = B_init_256[8], B9 = B_init_256[9], BA = B_init_256[10], BB = B_init_256[11],
        BC = B_init_256[12], BD = B_init_256[13], BE = B_init_256[14], BF = B_init_256[15];
    sph_u32
        C0 = C_init_256[0], C1 = C_init_256[1], C2 = C_init_256[2], C3 = C_init_256[3],
        C4 = C_init_256[4], C5 = C_init_256[5], C6 = C_init_256[6], C7 = C_init_256[7],
        C8 = C_init_256[8], C9 = C_init_256[9], CA = C_init_256[10], CB = C_init_256[11],
        CC = C_init_256[12], CD = C_init_256[13], CE = C_init_256[14], CF = C_init_256[15];
    sph_u32 M0, M1, M2, M3, M4, M5, M6, M7, M8, M9, MA, MB, MC, MD, ME, MF;
    sph_u32 Wlow = 1, Whigh = 0;

    uint full_blocks = len_words / 16;
    uint rem = len_words % 16;
    __global const sph_u32* p = msg;
    size_t step = (size_t)stride * 16;

    for (uint b = 0; b < full_blocks; b++) {
        M0 = p[0 * stride];  M1 = p[1 * stride];  M2 = p[2 * stride];  M3 = p[3 * stride];
        M4 = p[4 * stride];  M5 = p[5 * stride];  M6 = p[6 * stride];  M7 = p[7 * stride];
        M8 = p[8 * stride];  M9 = p[9 * stride];  MA = p[10 * stride]; MB = p[11 * stride];
        MC = p[12 * stride]; MD = p[13 * stride]; ME = p[14 * stride]; MF = p[15 * stride];
        INPUT_BLOCK_ADD;
        XOR_W;
        APPLY_P;
        INPUT_BLOCK_SUB;
        SWAP_BC;
        INCR_W;
        p += step;
    }

    /* Final block: the remaining words, then 0x80 and zero padding. */
    {
        sph_u32 w[16];
        for (uint k = 0; k < 16; k++) {
            w[k] = (k < rem) ? p[k * stride] : ((k == rem) ? 0x80U : 0U);
        }
        M0 = w[0];  M1 = w[1];  M2 = w[2];  M3 = w[3];
        M4 = w[4];  M5 = w[5];  M6 = w[6];  M7 = w[7];
        M8 = w[8];  M9 = w[9];  MA = w[10]; MB = w[11];
        MC = w[12]; MD = w[13]; ME = w[14]; MF = w[15];
    }
    INPUT_BLOCK_ADD;
    XOR_W;
    APPLY_P;
    for (uint i = 0; i < 3; i++) {
        SWAP_BC;
        XOR_W;
        APPLY_P;
    }

    out[0] = B8; out[1] = B9; out[2] = BA; out[3] = BB;
    out[4] = BC; out[5] = BD; out[6] = BE; out[7] = BF;
}

static inline uint bswap32(uint v)
{
    return (v >> 24) | ((v >> 8) & 0xFF00U) | ((v << 8) & 0xFF0000U) | (v << 24);
}

/*
 * gendata: GEN_WORDS * n words, interleaved (word w of work-item g at
 * gendata[w * n + g]).  finals: 8 * n words, same interleaving.  Work-items
 * >= count idle; n is the padded global size.
 */
__kernel void generate_nonces(ulong account_id, ulong start_nonce, uint count, uint n,
                              __global sph_u32* gendata, __global sph_u32* finals)
{
    uint gid = get_global_id(0);
    if (gid >= count) {
        return;
    }
    ulong nonce = start_nonce + gid;
    __global sph_u32* g = gendata + gid;
    size_t sn = n;

    /* Seed: account id and nonce, big-endian, in the last 16 bytes. */
    g[(GEN_WORDS - 4) * sn] = bswap32((uint)(account_id >> 32));
    g[(GEN_WORDS - 3) * sn] = bswap32((uint)(account_id & 0xFFFFFFFFUL));
    g[(GEN_WORDS - 2) * sn] = bswap32((uint)(nonce >> 32));
    g[(GEN_WORDS - 1) * sn] = bswap32((uint)(nonce & 0xFFFFFFFFUL));

    sph_u32 h[8];
    for (uint i = NONCE_SIZE; i > 0; i -= HASH_SIZE) {
        uint len = GEN_SIZE - i;
        if (len > HASH_CAP) {
            len = HASH_CAP;
        }
        shabal256_words(g + (size_t)(i / 4) * sn, n, len / 4, h);
        __global sph_u32* dst = g + (size_t)((i - HASH_SIZE) / 4) * sn;
        dst[0 * sn] = h[0]; dst[1 * sn] = h[1]; dst[2 * sn] = h[2]; dst[3 * sn] = h[3];
        dst[4 * sn] = h[4]; dst[5 * sn] = h[5]; dst[6 * sn] = h[6]; dst[7 * sn] = h[7];
    }

    shabal256_words(g, n, GEN_WORDS, h);
    for (uint k = 0; k < 8; k++) {
        finals[k * sn + gid] = h[k];
    }
}

/*
 * One work-item per (scoop, nonce); global size n * SCOOPS_PER_NONCE with the
 * nonce index varying fastest so the reads stay coalesced.  Writes the
 * finished scoop (final XOR, PoC2 swap) to out[(s * count + g) * 16 words].
 */
__kernel void finish_nonces(uint count, uint n, __global const sph_u32* gendata,
                            __global const sph_u32* finals, __global sph_u32* out)
{
    size_t id = get_global_id(0);
    uint g = (uint)(id % n);
    uint s = (uint)(id / n);
    if (g >= count || s >= SCOOPS_PER_NONCE) {
        return;
    }
    size_t sn = n;
    uint s2 = SCOOPS_PER_NONCE - 1 - s;
    __global const sph_u32* h0 = gendata + (size_t)(s * 16) * sn + g;
    __global const sph_u32* h1 = gendata + (size_t)(s2 * 16 + 8) * sn + g;
    __global sph_u32* dst = out + ((size_t)s * count + g) * 16;
    for (uint k = 0; k < 8; k++) {
        sph_u32 f = finals[k * sn + g];
        dst[k] = h0[k * sn] ^ f;
        dst[8 + k] = h1[k * sn] ^ f;
    }
}
