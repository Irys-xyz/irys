//! Prototype VDE (verifiable delay encoding) packing: slow to encode, fast to decode.
//!
//! Spec and slow reference: `scripts/vde_ref.py` (the known-answer tests below come from it).
//! One 256 KiB chunk is 8192 elements of 32 bytes, read as little-endian integers mod
//! p = 2^256 - 189 (prime, p = 3 mod 4). Each layer:
//!
//! 1. Root step, every element independently: `z = sloth(x + k[i] + layer)`. The Sloth
//!    permutation is a modular square root, `x^((p+1)/4)`: 253 squarings and 13 multiplies,
//!    strictly sequential per element. Its inverse is one squaring.
//! 2. Mix, with `lanes` lanes of `W = 8192 / lanes` steps each:
//!    `y[i] = z[i] + SHA256(y[i-1] || y[j]) mod p`, where `j` is chosen by `y[i-1]` among the
//!    elements finished at earlier steps (a data-dependent read, as in packing variant B).
//!    Encoding is sequential within a lane (`lanes = 1`: the whole chunk, `8192`: no mix);
//!    decoding needs only the stored `y`, so every element decodes independently.
//!
//! Capacity (empty space) is the encoding of zeros. Difficulty is the number of layers.
//! Prototype limitation: an input element must be < p (only crafted data can hit this).

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

/// Chunk bytes to elements. Every element must be < p.
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

#[inline(always)]
pub fn mul(a: &U256, b: &U256) -> U256 {
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

// ---- the encoding ------------------------------------------------------------------------

/// k[i] = SHA256(seed || LE32(i) || 0^28) mod p.
pub fn keys(id: &ChunkId) -> Elems {
    let seed = seed_hash(id);
    (0..SEGMENTS)
        .map(|i| {
            let mut m = [0_u8; 32];
            m[..4].copy_from_slice(&(i as u32).to_le_bytes());
            canonical(from_le_bytes(&sha256_64(&seed, &m)))
        })
        .collect()
}

/// Shape of the encoding.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Params {
    pub layers: u32,
    /// Mix lanes: 1 (one sequential pass over the chunk) ..= 8192 (no mix). A power of two.
    pub lanes: usize,
}

impl Params {
    fn width(&self) -> usize {
        assert!(
            self.lanes.is_power_of_two() && self.lanes <= SEGMENTS,
            "lanes must be a power of two <= 8192"
        );
        SEGMENTS / self.lanes
    }
}

#[inline(always)]
fn mix_index(prev: &U256, t: usize, lanes: usize, w: usize) -> usize {
    let m = (prev[0] as u32 as usize) % (t * lanes);
    (m % lanes) * w + m / lanes
}

#[inline(always)]
fn mix_hash(prev: &U256, other: &U256) -> U256 {
    canonical(from_le_bytes(&sha256_64(
        &to_le_bytes(prev),
        &to_le_bytes(other),
    )))
}

/// Root step of one layer over `xs[range]`: x = sloth(x + k + layer).
fn roots(xs: &mut [U256], ks: &[U256], layer: u32) {
    let l = [layer as u64, 0, 0, 0];
    for (x, k) in xs.iter_mut().zip(ks) {
        *x = sloth(&add(&add(x, k), &l));
    }
}

/// Mix step of one layer, in place (z -> y).
fn mix(ys: &mut [U256], lanes: usize, w: usize) {
    for t in 1..w {
        for lane in 0..lanes {
            let i = lane * w + t;
            let j = mix_index(&ys[i - 1], t, lanes, w);
            ys[i] = add(&ys[i], &mix_hash(&ys[i - 1], &ys[j]));
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

/// Encodes `xs` in place. The root step runs on `threads` threads (it is element-parallel);
/// the mix runs on the calling thread.
pub fn encode(id: &ChunkId, xs: &mut [U256], p: Params, threads: usize) -> Timing {
    assert_eq!(xs.len(), SEGMENTS);
    assert!(xs.iter().all(|x| !ge_p(x)), "input element >= p");
    let w = p.width();
    let mut timing = Timing::default();
    let t0 = std::time::Instant::now();
    let ks = keys(id);
    timing.keys_s = t0.elapsed().as_secs_f64();
    let per = SEGMENTS.div_ceil(threads.max(1));
    for layer in 0..p.layers {
        let t0 = std::time::Instant::now();
        if threads <= 1 {
            roots(xs, &ks, layer);
        } else {
            std::thread::scope(|s| {
                for (xc, kc) in xs.chunks_mut(per).zip(ks.chunks(per)) {
                    s.spawn(move || roots(xc, kc, layer));
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
    let ks = keys(id);
    for layer in (0..p.layers).rev() {
        // undo the mix from the stored y: walk t downwards so y[i-1] and y[j] (both at an
        // earlier t) are still unmodified when element i is restored
        for t in (1..w).rev() {
            for lane in 0..p.lanes {
                let i = lane * w + t;
                let j = mix_index(&ys[i - 1], t, p.lanes, w);
                let h = mix_hash(&ys[i - 1], &ys[j]);
                ys[i] = sub(&ys[i], &h);
            }
        }
        let l = [layer as u64, 0, 0, 0];
        for (y, k) in ys.iter_mut().zip(ks.iter()) {
            *y = sub(&sub(&sloth_inv(y), k), &l);
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
                "9759b214e1c66e3db29b054ee3feb778c1adb59931f15d8f165fba13cd1c1401",
            ),
            (
                64,
                "e3520e93c8cf5eb6c8acbd665ad6c2d75c930752c6915ce259e66f2019f64aab",
            ),
            (
                8192,
                "56fa200b82b22821cd686eb0c4a443ab2a394a40437712744a8c3dd3cfb01663",
            ),
        ] {
            let mut xs = zero_elems();
            encode(&id, &mut xs, Params { layers: 2, lanes }, 1);
            assert_eq!(digest(&xs), want, "lanes={lanes}");
        }
        let mut data: Elems = (0..SEGMENTS as u32)
            .map(|i| {
                let mut m = b"data".to_vec();
                m.extend_from_slice(&i.to_le_bytes());
                canonical(from_le_bytes(&openssl::sha::sha256(&m)))
            })
            .collect();
        encode(
            &id,
            &mut data,
            Params {
                layers: 1,
                lanes: 64,
            },
            1,
        );
        assert_eq!(
            digest(&data),
            "d68a44a57865057efe069db29e77991a41a03f01846ea2efbeb44e9bf7c1980d"
        );
    }

    #[test]
    fn decode_inverts_encode_and_threads_agree() {
        let mut rng = StdRng::seed_from_u64(5);
        let id = fixed_id(11);
        for lanes in [1, 2, 64, 4096, 8192] {
            let data: Elems = (0..SEGMENTS).map(|_| random_elem(&mut rng)).collect();
            let p = Params { layers: 2, lanes };
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
