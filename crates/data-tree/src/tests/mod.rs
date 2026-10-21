mod builder;
mod depth;
mod geometry;
mod geometry_props;
mod negative;
mod props;
mod roundtrip;
mod shape;
mod strategies;
mod tags;
mod vectors;

fn pattern(byte_length: usize) -> Vec<u8> {
    (0..byte_length)
        .map(|byte_index| u8::try_from(byte_index % 251).expect("251 fits in u8"))
        .collect()
}

fn assert_hex(actual: &[u8], hex_str: &str) {
    let expected = hex::decode(hex_str).unwrap_or_else(|err| panic!("hex {hex_str}: {err}"));
    assert_eq!(actual, expected.as_slice(), "{hex_str}");
}

fn concat_hex(lines: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend(hex::decode(line).unwrap_or_else(|err| panic!("hex {line}: {err}")));
    }
    out
}
