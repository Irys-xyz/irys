mod builder;
mod depth;
mod geometry;
mod negative;
mod props;
mod roundtrip;
mod shape;

fn pattern(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| u8::try_from(i % 251).expect("251 fits in u8"))
        .collect()
}
