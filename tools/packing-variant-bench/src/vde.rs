//! Prototype VDE (verifiable delay encoding) packing: slow to encode, fast to decode.
//!
//! Spec and slow reference: `scripts/vde_ref.py` (the known-answer tests below come from it).
//! One 256 KiB chunk is 8192 elements of 32 bytes, read as little-endian 256-bit integers.
//! The roots work mod p = 2^256 - 189 (prime, p = 3 mod 4); everything else is mod 2^256. Each
//! layer:
//!
//! 1. Root step: `z[i] = root(x[i] + k[i] + layer + z[i-1])`, the `z[i-1]` term only within one
//!    of `chains` chains of `8192 / chains` consecutive elements. `chains = 8192`: every root
//!    is independent. `chains = 1`: a layer is 8192 roots in series, so its encode latency is
//!    fixed by one core's root latency however much hardware is used. Decode recovers every
//!    `z` first, so it stays element-parallel either way. `root` is the
//!    Sloth permutation, a modular square root `x^((p+1)/4)` (253 squarings and 13 multiplies,
//!    strictly sequential per element; its inverse is one squaring), for values below p. The
//!    189 values from p up pass through unchanged, so any 32-byte input encodes and chunks keep
//!    their size. Crafted data can make layer 0 skip its roots; layer 0's mix then adds hashes
//!    it cannot steer.
//! 2. Mix, with `lanes` lanes of `W = 8192 / lanes` steps each:
//!    `y[i] = z[i] + SHA256(y[i-1] || y[j]) mod 2^256`, where `j` is chosen by `y[i-1]` among the
//!    elements finished at earlier steps (a data-dependent read, as in packing variant B).
//!    Encoding is sequential within a lane (`lanes = 1`: the whole chunk, `8192`: no mix);
//!    decoding needs only the stored `y`, so every element decodes independently.
//!
//! Capacity (empty space) is the encoding of zeros. Difficulty is the number of layers.

use crate::{CHUNK_SIZE, ChunkId, SEGMENTS, cpu_fast::sha256_64, seed_hash};

/// A field element as four little-endian 64-bit limbs.
pub type U256 = [u64; 4];
/// One chunk of field elements.
pub type Elems = Box<[U256]>;

/// p = 2^256 - 189.
pub const P: U256 = [0xFFFF_FFFF_FFFF_FF43, u64::MAX, u64::MAX, u64::MAX];
/// 2^256 mod p.
const C: u64 = 189;

pub fn zero_elems() -> Elems {
    vec![[0_u64; 4]; SEGMENTS].into_boxed_slice()
}

pub fn from_le_bytes(b: &[u8]) -> U256 {
    core::array::from_fn(|k| u64::from_le_bytes(b[8 * k..8 * k + 8].try_into().unwrap()))
}

pub fn to_le_bytes(x: &U256) -> [u8; 32] {
    let mut b = [0_u8; 32];
    for k in 0..4 {
        b[8 * k..8 * k + 8].copy_from_slice(&x[k].to_le_bytes());
    }
    b
}

/// Chunk bytes to elements.
pub fn elems_from_bytes(bytes: &[u8]) -> Elems {
    assert_eq!(bytes.len(), CHUNK_SIZE);
    bytes.chunks_exact(32).map(from_le_bytes).collect()
}

pub fn elems_to_bytes(xs: &[U256]) -> Vec<u8> {
    xs.iter().flat_map(to_le_bytes).collect()
}

// ---- arithmetic mod p --------------------------------------------------------------------

/// 512-bit product to a value < 2^256 that is congruent mod p (not necessarily < p).
#[inline(always)]
fn reduce(t: [u64; 8]) -> U256 {
    let mut r = [0_u64; 4];
    let mut carry: u128 = 0;
    for i in 0..4 {
        let v = t[i] as u128 + (t[i + 4] as u128) * C as u128 + carry;
        r[i] = v as u64;
        carry = v >> 64;
    }
    // carry <= 189: fold it in once more
    let mut c = carry * C as u128;
    for limb in &mut r {
        let v = *limb as u128 + c;
        *limb = v as u64;
        c = v >> 64;
    }
    if c != 0 {
        // wrapped past 2^256: the value is now tiny, add 2^256 mod p
        r[0] += C;
    }
    r
}

/// Whether `mul` / `sqr` use the MULX/ADX assembly: only with `PVB_VDE_CPU_MUL=adx` on x86_64
/// with BMI2 + ADX. Read once. Off by default: it measured no faster than the portable code,
/// which LLVM already compiles to MULX (~12 ns per dependent squaring either way on Zen 5).
pub fn use_adx() -> bool {
    static USE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *USE.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            std::env::var("PVB_VDE_CPU_MUL").as_deref() == Ok("adx")
                && is_x86_feature_detected!("bmi2")
                && is_x86_feature_detected!("adx")
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            false
        }
    })
}

/// 256x256 -> 512-bit product with MULX and two carry chains (ADCX on CF, ADOX on OF), one row
/// per limb of `b`. Five rolling registers hold the row window; finished limbs go to `out`.
///
/// # Safety
/// The CPU must support BMI2 and ADX.
#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "bmi2,adx")]
unsafe fn mul_wide_adx(a: &U256, b: &U256) -> [u64; 8] {
    let mut out = [0_u64; 8];
    // SAFETY: reads 4 limbs from each of a and b, writes 8 limbs to out; no stack use.
    unsafe {
        core::arch::asm!(
            // row 0: t0..t4 = r8, r9, r10, r11, r12
            "mov rdx, qword ptr [rcx]",
            "mulx r9, r8, qword ptr [rsi]",
            "mulx r10, r14, qword ptr [rsi + 8]",
            "add r9, r14",
            "mulx r11, r14, qword ptr [rsi + 16]",
            "adc r10, r14",
            "mulx r12, r14, qword ptr [rsi + 24]",
            "adc r11, r14",
            "adc r12, 0",
            "mov qword ptr [rdi], r8",
            // row 1: t1..t5 = r9, r10, r11, r12, r8
            "mov rdx, qword ptr [rcx + 8]",
            "xor r15, r15",
            "mulx r13, r14, qword ptr [rsi]",
            "adcx r9, r14",
            "adox r10, r13",
            "mulx r13, r14, qword ptr [rsi + 8]",
            "adcx r10, r14",
            "adox r11, r13",
            "mulx r13, r14, qword ptr [rsi + 16]",
            "adcx r11, r14",
            "adox r12, r13",
            "mulx r8, r14, qword ptr [rsi + 24]",
            "adcx r12, r14",
            "adox r8, r15",
            "adcx r8, r15",
            "mov qword ptr [rdi + 8], r9",
            // row 2: t2..t6 = r10, r11, r12, r8, r9
            "mov rdx, qword ptr [rcx + 16]",
            "xor r15, r15",
            "mulx r13, r14, qword ptr [rsi]",
            "adcx r10, r14",
            "adox r11, r13",
            "mulx r13, r14, qword ptr [rsi + 8]",
            "adcx r11, r14",
            "adox r12, r13",
            "mulx r13, r14, qword ptr [rsi + 16]",
            "adcx r12, r14",
            "adox r8, r13",
            "mulx r9, r14, qword ptr [rsi + 24]",
            "adcx r8, r14",
            "adox r9, r15",
            "adcx r9, r15",
            "mov qword ptr [rdi + 16], r10",
            // row 3: t3..t7 = r11, r12, r8, r9, r10
            "mov rdx, qword ptr [rcx + 24]",
            "xor r15, r15",
            "mulx r13, r14, qword ptr [rsi]",
            "adcx r11, r14",
            "adox r12, r13",
            "mulx r13, r14, qword ptr [rsi + 8]",
            "adcx r12, r14",
            "adox r8, r13",
            "mulx r13, r14, qword ptr [rsi + 16]",
            "adcx r8, r14",
            "adox r9, r13",
            "mulx r10, r14, qword ptr [rsi + 24]",
            "adcx r9, r14",
            "adox r10, r15",
            "adcx r10, r15",
            "mov qword ptr [rdi + 24], r11",
            "mov qword ptr [rdi + 32], r12",
            "mov qword ptr [rdi + 40], r8",
            "mov qword ptr [rdi + 48], r9",
            "mov qword ptr [rdi + 56], r10",
            in("rsi") a.as_ptr(),
            in("rcx") b.as_ptr(),
            in("rdi") out.as_mut_ptr(),
            out("rdx") _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("r11") _,
            out("r12") _,
            out("r13") _,
            out("r14") _,
            out("r15") _,
            options(nostack),
        );
    }
    out
}

/// a^2 mod p as a value < 2^256 (same contract as `reduce`), all in registers: 6 cross products
/// (MULX, ADCX/ADOX), doubled, plus 4 squares, then the 2^256 = 189 fold and a second fold.
///
/// # Safety
/// The CPU must support BMI2 and ADX.
#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "bmi2,adx")]
unsafe fn sqr_adx(a: &U256) -> U256 {
    let mut out = [0_u64; 4];
    // SAFETY: reads 4 limbs of a, writes 4 limbs of out; no stack use.
    unsafe {
        core::arch::asm!(
            // cross products into t1..t6 = r9..r14
            "mov rdx, qword ptr [rsi]",
            "mulx r10, r9, qword ptr [rsi + 8]",
            "mulx r11, rcx, qword ptr [rsi + 16]",
            "add r10, rcx",
            "mulx r12, rcx, qword ptr [rsi + 24]",
            "adc r11, rcx",
            "adc r12, 0",
            "mov rdx, qword ptr [rsi + 8]",
            "xor eax, eax",
            "mulx rax, rcx, qword ptr [rsi + 16]",
            "adcx r11, rcx",
            "adox r12, rax",
            "mulx r13, rcx, qword ptr [rsi + 24]",
            "adcx r12, rcx",
            "mov ecx, 0",
            "adox r13, rcx",
            "adcx r13, rcx",
            "mov rdx, qword ptr [rsi + 16]",
            "mulx r14, rcx, qword ptr [rsi + 24]",
            "add r13, rcx",
            "adc r14, 0",
            // double t1..t6, carry into t7 = r15
            "xor r15d, r15d",
            "add r9, r9",
            "adc r10, r10",
            "adc r11, r11",
            "adc r12, r12",
            "adc r13, r13",
            "adc r14, r14",
            "adc r15, r15",
            // squares at limbs 0, 2, 4, 6 (t0 = r8)
            "mov rdx, qword ptr [rsi]",
            "mulx rax, r8, rdx",
            "add r9, rax",
            "mov rdx, qword ptr [rsi + 8]",
            "mulx rax, rcx, rdx",
            "adc r10, rcx",
            "adc r11, rax",
            "mov rdx, qword ptr [rsi + 16]",
            "mulx rax, rcx, rdx",
            "adc r12, rcx",
            "adc r13, rax",
            "mov rdx, qword ptr [rsi + 24]",
            "mulx rax, rcx, rdx",
            "adc r14, rcx",
            "adc r15, rax",
            // t0..t3 += 189 * t4..t7, top word into r12
            "mov edx, 189",
            "xor eax, eax",
            "mulx rax, rcx, r12",
            "adcx r8, rcx",
            "adox r9, rax",
            "mulx rax, rcx, r13",
            "adcx r9, rcx",
            "adox r10, rax",
            "mulx rax, rcx, r14",
            "adcx r10, rcx",
            "adox r11, rax",
            "mulx r12, rcx, r15",
            "adcx r11, rcx",
            "mov ecx, 0",
            "adox r12, rcx",
            "adcx r12, rcx",
            // fold the top word (<= 191) once more; a final wrap leaves a tiny value, add 189
            "imul r12, r12, 189",
            "add r8, r12",
            "adc r9, 0",
            "adc r10, 0",
            "adc r11, 0",
            "mov ecx, 0",
            "mov eax, 189",
            "cmovc rcx, rax",
            "add r8, rcx",
            "mov qword ptr [rdi], r8",
            "mov qword ptr [rdi + 8], r9",
            "mov qword ptr [rdi + 16], r10",
            "mov qword ptr [rdi + 24], r11",
            in("rsi") a.as_ptr(),
            in("rdi") out.as_mut_ptr(),
            out("rax") _,
            out("rcx") _,
            out("rdx") _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("r11") _,
            out("r12") _,
            out("r13") _,
            out("r14") _,
            out("r15") _,
            options(nostack),
        );
    }
    out
}

#[inline(always)]
pub fn mul(a: &U256, b: &U256) -> U256 {
    #[cfg(target_arch = "x86_64")]
    if use_adx() {
        // SAFETY: use_adx() checked BMI2 and ADX.
        return reduce(unsafe { mul_wide_adx(a, b) });
    }
    mul_portable(a, b)
}

#[inline(always)]
fn mul_portable(a: &U256, b: &U256) -> U256 {
    let mut t = [0_u64; 8];
    for i in 0..4 {
        let mut carry: u128 = 0;
        for j in 0..4 {
            let v = t[i + j] as u128 + (a[i] as u128) * (b[j] as u128) + carry;
            t[i + j] = v as u64;
            carry = v >> 64;
        }
        t[i + 4] = carry as u64;
    }
    reduce(t)
}

#[inline(always)]
pub fn sqr(a: &U256) -> U256 {
    #[cfg(target_arch = "x86_64")]
    if use_adx() {
        // SAFETY: use_adx() checked BMI2 and ADX.
        return unsafe { sqr_adx(a) };
    }
    sqr_portable(a)
}

/// Squaring with the MULX/ADX assembly when the CPU supports it, else portable (benchmarks).
pub fn sqr_asm_or_portable(a: &U256) -> U256 {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("bmi2") && is_x86_feature_detected!("adx") {
        // SAFETY: BMI2 and ADX checked.
        return unsafe { sqr_adx(a) };
    }
    sqr_portable(a)
}

/// Portable squaring (benchmarks).
#[inline(always)]
pub fn sqr_portable(a: &U256) -> U256 {
    // cross products once, doubled, plus the squares: 10 multiplies instead of 16
    let mut t = [0_u64; 8];
    for i in 0..4 {
        let mut carry: u128 = 0;
        for j in i + 1..4 {
            let v = t[i + j] as u128 + (a[i] as u128) * (a[j] as u128) + carry;
            t[i + j] = v as u64;
            carry = v >> 64;
        }
        t[i + 4] = carry as u64;
    }
    let mut top = 0_u64;
    for limb in &mut t {
        let next = *limb >> 63;
        *limb = (*limb << 1) | top;
        top = next;
    }
    let mut carry: u128 = 0;
    for i in 0..4 {
        let sq = (a[i] as u128) * (a[i] as u128);
        let lo = t[2 * i] as u128 + (sq as u64) as u128 + carry;
        t[2 * i] = lo as u64;
        let hi = t[2 * i + 1] as u128 + (sq >> 64) + (lo >> 64);
        t[2 * i + 1] = hi as u64;
        carry = hi >> 64;
    }
    reduce(t)
}

#[inline(always)]
fn ge_p(x: &U256) -> bool {
    x[3] == u64::MAX && x[2] == u64::MAX && x[1] == u64::MAX && x[0] >= P[0]
}

/// The representative in [0, p) of a value < 2^256.
#[inline(always)]
pub fn canonical(mut x: U256) -> U256 {
    if ge_p(&x) {
        // x - p = x + 189 - 2^256, and x >= p means x + 189 overflows exactly once
        let mut c = C as u128;
        for limb in &mut x {
            let v = *limb as u128 + c;
            *limb = v as u64;
            c = v >> 64;
        }
    }
    x
}

/// (a + b) mod p for a, b < p.
#[inline(always)]
pub fn add(a: &U256, b: &U256) -> U256 {
    let mut s = [0_u64; 4];
    let mut c: u128 = 0;
    for k in 0..4 {
        let v = a[k] as u128 + b[k] as u128 + c;
        s[k] = v as u64;
        c = v >> 64;
    }
    if c != 0 {
        // a + b - p = (a + b - 2^256) + 189 < p
        let mut c = C as u128;
        for limb in &mut s {
            let v = *limb as u128 + c;
            *limb = v as u64;
            c = v >> 64;
        }
        s
    } else {
        canonical(s)
    }
}

/// (a - b) mod p for a, b < p.
#[inline(always)]
pub fn sub(a: &U256, b: &U256) -> U256 {
    let mut d = [0_u64; 4];
    let mut borrow = false;
    for k in 0..4 {
        let (v, b1) = a[k].overflowing_sub(b[k]);
        let (v, b2) = v.overflowing_sub(borrow as u64);
        d[k] = v;
        borrow = b1 | b2;
    }
    if borrow {
        // d + p = d - 189 mod 2^256
        let mut br = C;
        for limb in &mut d {
            let (v, b) = limb.overflowing_sub(br);
            *limb = v;
            br = b as u64;
        }
    }
    d
}

/// (a + b) mod 2^256.
#[inline(always)]
pub fn add_wrap(a: &U256, b: &U256) -> U256 {
    let mut s = [0_u64; 4];
    let mut c = false;
    for k in 0..4 {
        let (v, c1) = a[k].overflowing_add(b[k]);
        let (v, c2) = v.overflowing_add(c as u64);
        s[k] = v;
        c = c1 | c2;
    }
    s
}

/// (a - b) mod 2^256.
#[inline(always)]
pub fn sub_wrap(a: &U256, b: &U256) -> U256 {
    let mut d = [0_u64; 4];
    let mut borrow = false;
    for k in 0..4 {
        let (v, b1) = a[k].overflowing_sub(b[k]);
        let (v, b2) = v.overflowing_sub(borrow as u64);
        d[k] = v;
        borrow = b1 | b2;
    }
    d
}

#[inline(always)]
fn neg(x: &U256) -> U256 {
    sub(&[0; 4], x)
}

#[inline(always)]
fn sqr_n(mut x: U256, n: u32) -> U256 {
    for _ in 0..n {
        x = sqr(&x);
    }
    x
}

/// x^((p+1)/4) = x^(2^254 - 47): 248 one bits, then 010001. 253 squarings, 13 multiplies.
#[inline]
pub fn pow_sqrt(x: &U256) -> U256 {
    let x2 = mul(&sqr(x), x); // 2^2 - 1
    let x4 = mul(&sqr_n(x2, 2), &x2);
    let x8 = mul(&sqr_n(x4, 4), &x4);
    let x16 = mul(&sqr_n(x8, 8), &x8);
    let x32 = mul(&sqr_n(x16, 16), &x16);
    let x64 = mul(&sqr_n(x32, 32), &x32);
    let x128 = mul(&sqr_n(x64, 64), &x64);
    let x192 = mul(&sqr_n(x128, 64), &x64);
    let x224 = mul(&sqr_n(x192, 32), &x32);
    let x240 = mul(&sqr_n(x224, 16), &x16);
    let x248 = mul(&sqr_n(x240, 8), &x8);
    // low bits 010001
    let mut acc = sqr_n(x248, 2); // bits 0, 1
    acc = mul(&acc, x);
    acc = sqr_n(acc, 4); // bits 0, 0, 0, 1
    acc = mul(&acc, x);
    canonical(acc)
}

/// Sloth permutation of [0, p): the even square root of x if x is a square, else the odd
/// square root of -x.
#[inline]
pub fn sloth(x: &U256) -> U256 {
    let r = pow_sqrt(x);
    let is_square = canonical(sqr(&r)) == *x;
    let r_even = r[0] & 1 == 0;
    if is_square == r_even { r } else { neg(&r) }
}

/// Inverse of [`sloth`]: one squaring.
#[inline]
pub fn sloth_inv(y: &U256) -> U256 {
    let s = canonical(sqr(y));
    if y[0] & 1 == 0 { s } else { neg(&s) }
}

/// The root step's permutation of all 2^256 values: [`sloth`] below p, identity from p up.
#[inline]
pub fn root(x: &U256) -> U256 {
    if ge_p(x) { *x } else { sloth(x) }
}

/// Inverse of [`root`] (sloth maps [0, p) onto itself, so values from p up are fixed points).
#[inline]
pub fn root_inv(y: &U256) -> U256 {
    if ge_p(y) { *y } else { sloth_inv(y) }
}

// ---- the encoding ------------------------------------------------------------------------

/// k[i] = SHA256(seed || LE32(i) || 0^28).
pub fn keys(id: &ChunkId) -> Elems {
    let seed = seed_hash(id);
    (0..SEGMENTS)
        .map(|i| {
            let mut m = [0_u8; 32];
            m[..4].copy_from_slice(&(i as u32).to_le_bytes());
            from_le_bytes(&sha256_64(&seed, &m))
        })
        .collect()
}

/// Shape of the encoding.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Params {
    pub layers: u32,
    /// Mix lanes: 1 (one sequential pass over the chunk) ..= 8192 (no mix). A power of two.
    pub lanes: usize,
    /// Root chains: 1 (all roots of a layer in series) ..= 8192 (independent). A power of two.
    pub chains: usize,
}

impl Params {
    fn width(&self) -> usize {
        assert!(
            self.lanes.is_power_of_two() && self.lanes <= SEGMENTS,
            "lanes must be a power of two <= 8192"
        );
        SEGMENTS / self.lanes
    }

    fn chain_len(&self) -> usize {
        assert!(
            self.chains.is_power_of_two() && self.chains <= SEGMENTS,
            "chains must be a power of two <= 8192"
        );
        SEGMENTS / self.chains
    }
}

#[inline(always)]
fn mix_index(prev: &U256, t: usize, lanes: usize, w: usize) -> usize {
    let m = (prev[0] as u32 as usize) % (t * lanes);
    (m % lanes) * w + m / lanes
}

#[inline(always)]
fn mix_hash(prev: &U256, other: &U256) -> U256 {
    from_le_bytes(&sha256_64(&to_le_bytes(prev), &to_le_bytes(other)))
}

/// Root step of one layer over `xs`, which starts at a chain boundary:
/// z[i] = root(x[i] + k[i] + layer + z[i-1]), the last term only within a chain of `wc` elements.
fn roots(xs: &mut [U256], ks: &[U256], layer: u32, wc: usize) {
    let l = [layer as u64, 0, 0, 0];
    for i in 0..xs.len() {
        let mut v = add_wrap(&add_wrap(&xs[i], &ks[i]), &l);
        if i % wc != 0 {
            v = add_wrap(&v, &xs[i - 1]);
        }
        xs[i] = root(&v);
    }
}

/// Mix step of one layer, in place (z -> y).
fn mix(ys: &mut [U256], lanes: usize, w: usize) {
    for t in 1..w {
        for lane in 0..lanes {
            let i = lane * w + t;
            let j = mix_index(&ys[i - 1], t, lanes, w);
            ys[i] = add_wrap(&ys[i], &mix_hash(&ys[i - 1], &ys[j]));
        }
    }
}

/// Time spent in each step of an encode, for the benchmark.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct Timing {
    pub keys_s: f64,
    pub roots_s: f64,
    pub mix_s: f64,
}

/// Encodes `xs` in place. The root step runs on up to `threads` threads (one or more whole
/// chains each); the mix runs on the calling thread.
pub fn encode(id: &ChunkId, xs: &mut [U256], p: Params, threads: usize) -> Timing {
    assert_eq!(xs.len(), SEGMENTS);
    let w = p.width();
    let wc = p.chain_len();
    let mut timing = Timing::default();
    let t0 = std::time::Instant::now();
    let ks = keys(id);
    timing.keys_s = t0.elapsed().as_secs_f64();
    let per = SEGMENTS.div_ceil(threads.max(1)).next_multiple_of(wc);
    for layer in 0..p.layers {
        let t0 = std::time::Instant::now();
        if threads <= 1 {
            roots(xs, &ks, layer, wc);
        } else {
            std::thread::scope(|s| {
                for (xc, kc) in xs.chunks_mut(per).zip(ks.chunks(per)) {
                    s.spawn(move || roots(xc, kc, layer, wc));
                }
            });
        }
        let t1 = std::time::Instant::now();
        mix(xs, p.lanes, w);
        timing.roots_s += (t1 - t0).as_secs_f64();
        timing.mix_s += t1.elapsed().as_secs_f64();
    }
    timing
}

/// Decodes `ys` in place, single-threaded. Every element is independent, so this would split
/// across threads (or SIMD lanes) trivially; one thread gives the per-core cost.
pub fn decode(id: &ChunkId, ys: &mut [U256], p: Params) {
    assert_eq!(ys.len(), SEGMENTS);
    let w = p.width();
    let wc = p.chain_len();
    let ks = keys(id);
    for layer in (0..p.layers).rev() {
        // undo the mix from the stored y: walk t downwards so y[i-1] and y[j] (both at an
        // earlier t) are still unmodified when element i is restored
        for t in (1..w).rev() {
            for lane in 0..p.lanes {
                let i = lane * w + t;
                let j = mix_index(&ys[i - 1], t, p.lanes, w);
                let h = mix_hash(&ys[i - 1], &ys[j]);
                ys[i] = sub_wrap(&ys[i], &h);
            }
        }
        // undo the roots from the recovered z: walk i downwards so z[i-1] is still unmodified
        let l = [layer as u64, 0, 0, 0];
        for i in (0..SEGMENTS).rev() {
            let mut x = sub_wrap(&sub_wrap(&root_inv(&ys[i]), &ks[i]), &l);
            if i % wc != 0 {
                x = sub_wrap(&x, &ys[i - 1]);
            }
            ys[i] = x;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed_id;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn random_elem(rng: &mut StdRng) -> U256 {
        loop {
            let x: U256 = core::array::from_fn(|_| rng.r#gen());
            if !ge_p(&x) {
                return x;
            }
        }
    }

    /// Slow reference: binary exponentiation with `mul` only.
    fn pow_ref(x: &U256, e: &U256) -> U256 {
        let mut acc = [1, 0, 0, 0];
        for k in (0..256).rev() {
            acc = mul(&acc, &acc);
            if (e[k / 64] >> (k % 64)) & 1 == 1 {
                acc = mul(&acc, x);
            }
        }
        canonical(acc)
    }

    #[test]
    fn sqr_matches_mul_and_edge_values() {
        let mut rng = StdRng::seed_from_u64(1);
        let edges = [
            [0; 4],
            [1, 0, 0, 0],
            neg(&[1, 0, 0, 0]),
            [u64::MAX; 4],
            [P[0] - 1, u64::MAX, u64::MAX, u64::MAX],
        ];
        for x in edges
            .into_iter()
            .chain((0..10_000).map(|_| random_elem(&mut rng)))
        {
            assert_eq!(canonical(sqr(&x)), canonical(mul(&x, &x)));
        }
        // (p - 1)^2 = 1
        assert_eq!(canonical(sqr(&neg(&[1, 0, 0, 0]))), [1, 0, 0, 0]);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn adx_multiply_matches_portable() {
        if !(is_x86_feature_detected!("bmi2") && is_x86_feature_detected!("adx")) {
            return;
        }
        let mut rng = StdRng::seed_from_u64(9);
        let edges = [
            [0; 4],
            [1, 0, 0, 0],
            [u64::MAX; 4],
            P,
            [u64::MAX, 0, u64::MAX, 0],
        ];
        let values: Vec<U256> = edges
            .into_iter()
            .chain((0..20_000).map(|_| core::array::from_fn(|_| rng.r#gen())))
            .collect();
        for w in values.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            // SAFETY: BMI2 and ADX checked above.
            let wide = unsafe { mul_wide_adx(a, b) };
            assert_eq!(canonical(reduce(wide)), canonical(mul_portable(a, b)));
            // SAFETY: as above.
            let sq = unsafe { sqr_adx(a) };
            assert_eq!(canonical(sq), canonical(sqr_portable(a)));
        }
    }

    #[test]
    fn fermat_and_sqrt_exponent() {
        let mut rng = StdRng::seed_from_u64(2);
        let p_minus_1 = sub(&[0; 4], &[1, 0, 0, 0]);
        let e = [
            0xFFFF_FFFF_FFFF_FFD1,
            u64::MAX,
            u64::MAX,
            0x3FFF_FFFF_FFFF_FFFF,
        ]; // 2^254 - 47
        for _ in 0..50 {
            let x = random_elem(&mut rng);
            assert_eq!(pow_ref(&x, &p_minus_1), [1, 0, 0, 0], "x^(p-1) = 1");
            assert_eq!(pow_sqrt(&x), pow_ref(&x, &e), "addition chain == (p+1)/4");
        }
    }

    #[test]
    fn add_sub_inverse() {
        let mut rng = StdRng::seed_from_u64(3);
        for _ in 0..10_000 {
            let (a, b) = (random_elem(&mut rng), random_elem(&mut rng));
            let s = add(&a, &b);
            assert!(!ge_p(&s));
            assert_eq!(sub(&s, &b), a);
        }
    }

    #[test]
    fn sloth_is_a_permutation_with_cheap_inverse() {
        let mut rng = StdRng::seed_from_u64(4);
        let edges = [
            [0; 4],
            [1, 0, 0, 0],
            [2, 0, 0, 0],
            neg(&[1, 0, 0, 0]),
            neg(&[2, 0, 0, 0]),
        ];
        for x in edges
            .into_iter()
            .chain((0..2_000).map(|_| random_elem(&mut rng)))
        {
            let y = sloth(&x);
            assert!(!ge_p(&y));
            assert_eq!(sloth_inv(&y), x);
        }
    }

    fn digest(xs: &[U256]) -> String {
        crate::to_hex(&openssl::sha::sha256(&elems_to_bytes(xs)))
    }

    /// Known answers from `scripts/vde_ref.py`.
    #[test]
    fn matches_python_reference() {
        let id = fixed_id(7);
        for (lanes, want) in [
            (
                1,
                "0c78bb9f5f4fdffc5224f97de61ae0468e79df62718a8c8ef9687f21dbe060bb",
            ),
            (
                64,
                "c08575b6dcf76f8675026fac4dbc15ca45d820f1c121a478dbc9d0b3f89df169",
            ),
            (
                8192,
                "078c6d935c2f4d84c8c2f77d781e25e25e0dcea4fd0d6c6779af45cbafd0f8fa",
            ),
        ] {
            let mut xs = zero_elems();
            encode(
                &id,
                &mut xs,
                Params {
                    layers: 2,
                    lanes,
                    chains: SEGMENTS,
                },
                1,
            );
            assert_eq!(digest(&xs), want, "lanes={lanes}");
        }
        let mut data: Elems = (0..SEGMENTS as u32)
            .map(|i| {
                let mut m = b"data".to_vec();
                m.extend_from_slice(&i.to_le_bytes());
                from_le_bytes(&openssl::sha::sha256(&m))
            })
            .collect();
        encode(
            &id,
            &mut data,
            Params {
                layers: 1,
                lanes: 64,
                chains: SEGMENTS,
            },
            1,
        );
        assert_eq!(
            digest(&data),
            "9f7468553ee444a1a06e47482590dbc96dfc306859fac79bb33d4ba9f8a4b634"
        );
    }

    /// Chained roots: known answers from `scripts/vde_ref.py`, and the round trip.
    #[test]
    fn chained_roots_match_python_reference() {
        let id = fixed_id(7);
        let data: Elems = (0..SEGMENTS as u32)
            .map(|i| {
                let mut m = b"data".to_vec();
                m.extend_from_slice(&i.to_le_bytes());
                from_le_bytes(&openssl::sha::sha256(&m))
            })
            .collect();
        for (chains, want) in [
            (
                1,
                "d1aec72239b4579cb5d47d46ad652cb0af396594ac7092c6961eec783d9fd8c2",
            ),
            (
                64,
                "20fac2b7706d07b21ebd4f532efe3f1c9d74b99ceac355beb3ccadb4558618bb",
            ),
        ] {
            let p = Params {
                layers: 2,
                lanes: 64,
                chains,
            };
            let mut xs = data.clone();
            encode(&id, &mut xs, p, 1);
            assert_eq!(digest(&xs), want, "chains={chains}");
            let mut ys = data.clone();
            encode(&id, &mut ys, p, 5);
            assert_eq!(ys, xs, "chains={chains}: thread count changed the output");
            decode(&id, &mut xs, p);
            assert_eq!(xs, data, "chains={chains}");
        }
    }

    /// Crafted data: every element is at or above p after layer 0's key, so layer 0's roots
    /// pass through. It must still round-trip, and match the reference.
    #[test]
    fn crafted_data_at_or_above_p() {
        let id = fixed_id(7);
        let ks = keys(&id);
        let crafted: Elems = (0..SEGMENTS)
            .map(|i| sub_wrap(&add_wrap(&P, &[(i % 189) as u64, 0, 0, 0]), &ks[i]))
            .collect();
        let p = Params {
            layers: 2,
            lanes: 64,
            chains: SEGMENTS,
        };
        let mut xs = crafted.clone();
        encode(&id, &mut xs, p, 1);
        assert_eq!(
            digest(&xs),
            "84b98f8b2b4a9efc4b607b529391cd3238163e40bbb7cd267150dbf247916f67"
        );
        decode(&id, &mut xs, p);
        assert_eq!(xs, crafted);
    }

    #[test]
    fn root_permutes_every_value() {
        let mut rng = StdRng::seed_from_u64(7);
        let top = (0..189).map(|k| add_wrap(&P, &[k, 0, 0, 0]));
        for x in top.chain((0..1_000).map(|_| random_elem(&mut rng))) {
            assert_eq!(root_inv(&root(&x)), x);
        }
        assert_eq!(root(&[u64::MAX; 4]), [u64::MAX; 4]);
    }

    #[test]
    fn decode_inverts_encode_and_threads_agree() {
        let mut rng = StdRng::seed_from_u64(5);
        let id = fixed_id(11);
        for lanes in [1, 2, 64, 4096, 8192] {
            // any 32-byte values, including some at or above p
            let mut data: Elems = (0..SEGMENTS)
                .map(|_| core::array::from_fn(|_| rng.r#gen()))
                .collect();
            for k in 0..64 {
                data[k * 97] = add_wrap(&P, &[k as u64, 0, 0, 0]);
            }
            let p = Params {
                layers: 2,
                lanes,
                chains: SEGMENTS,
            };
            let (mut a, mut b) = (data.clone(), data.clone());
            encode(&id, &mut a, p, 1);
            encode(&id, &mut b, p, 7);
            assert_eq!(a, b, "lanes={lanes}: thread count changed the output");
            assert_ne!(a, data);
            decode(&id, &mut a, p);
            assert_eq!(a, data, "lanes={lanes}");
        }
    }

    #[test]
    fn sha256_64_matches_openssl() {
        let mut rng = StdRng::seed_from_u64(6);
        for _ in 0..1000 {
            let (l, r): ([u8; 32], [u8; 32]) = (rng.r#gen(), rng.r#gen());
            let mut h = openssl::sha::Sha256::new();
            h.update(&l);
            h.update(&r);
            assert_eq!(sha256_64(&l, &r), h.finish());
        }
    }
}
