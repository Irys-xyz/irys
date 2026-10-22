//! Checked-in hashes and roots from spec §13. The numbers in the rows are the
//! vector inputs; the hex strings are the expected digests.

use super::{assert_hex, concat_hex, pattern};
use crate::DataTree;
use crate::MAX_SUBTREE_HEIGHT;
use crate::geometry::Geometry;
use crate::hash::{self, empty, leaf, size_witness};
use crate::types::DataTreeRoot;
use crate::types::HASH_SIZE;
use crate::verify::{verify_chunk, verify_segment};

#[test]
fn primitive_vectors() {
    assert_hex(
        leaf(b"").as_bytes(),
        "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
    );
    assert_eq!(empty(0), leaf(b""));
    assert_hex(
        empty(6).as_bytes(),
        "2be3cf0551cc6936d461e3dc43f3c4bf50cbee1bc091925254e879f4e7665e94",
    );
    for empty_height in 1..=MAX_SUBTREE_HEIGHT {
        assert_eq!(
            empty(empty_height),
            hash::node(&empty(empty_height - 1), &empty(empty_height - 1))
        );
    }
    assert_hex(
        hash::size_witness(1).as_bytes(),
        "f83f60940c1ec44c0f1e90f694c6cae7c99b4b6d1507d60ad5a3282a7750d0ee",
    );
    assert_hex(
        hash::size_witness(4096).as_bytes(),
        "719e72af15abf7c6f95ea297c02cd24b41b6b3467a78583a7103dbb19d45571f",
    );
    assert_hex(
        hash::size_witness(262_144).as_bytes(),
        "313eba86534a86e9270cfa6cb7cee900b753358a0c5ef61330202481653d22eb",
    );
    assert_hex(
        hash::size_witness(917_504).as_bytes(),
        "42bb7029ceeb50156a1b044c1af62a0d78207b52708ff5c31a78ed81c2bd595b",
    );

    // spec §4: the commitment is node(data_tree_root, size_witness).
    // Any filler byte. The check is the commitment formula, not this value.
    let root = DataTreeRoot::from([9_u8; HASH_SIZE]);
    let data_size = 123_u64;
    let via_node = hash::node(
        &crate::types::NodeHash::from(*root.as_bytes()),
        &crate::types::NodeHash::from(*hash::size_witness(data_size).as_bytes()),
    );
    assert_eq!(
        hash::data_commitment(&root, data_size).as_bytes(),
        via_node.as_bytes()
    );
}

#[test]
#[should_panic(expected = "exceeds 8")]
fn empty_above_8_panics() {
    let _ = empty(MAX_SUBTREE_HEIGHT + 1);
}

#[test]
fn testing_root_vectors() {
    let rows: &[(u64, &str, &str)] = &[
        (
            1,
            "8521b87ff724236722cfb6d6306ad59bceb4aee3e35ae73c41326cd31915e13e",
            "a107920f86fd22b8f786cace824308964a955e019625b0246a2caf3cdd122eb6",
        ),
        (
            8,
            "2239889b93ac82ead2db9605f2584a25255c90206c1fccaddf54caa75b7fc1a1",
            "87bbfb2c56ebf02b55bd51d32633445fe986f4031be1a3d04d09fac9d447106e",
        ),
        (
            9,
            "3ba2878285288e004dcd35a73e9a3df4a19f5fe5acccaf8d84b50fc7254e6ddb",
            "55100cd7d0c2bdbc9d03808eebcde5d5d144f27e021e3477c8a84834d5a0de94",
        ),
        (
            32,
            "d3e81f198d6bd3782d1941172c89e49721d8f5e33cc709d4f6ba3f54b9e35f82",
            "00f0535265357690ad8c75c88235490e7340e4cef3a81157c2c473861866d8d7",
        ),
        (
            33,
            "bf98e27c471bfda3de1d44b4eb420f528238aa75e6e002fe1636c04a431713a9",
            "ca888a5334e649c2c8a6ddece5b132464e67b1f59f0b5b5d02433070cce02fcf",
        ),
        (
            112,
            "7fd698c547deb0bbb8f8ab7db08dd34616a496f141167252487509cb74824564",
            "cf1fcae00134fbb7e7317da641c092740b22136c469ae46b6d1db03c4f01dbe5",
        ),
        (
            157,
            "81e7a996cd2ba141815aa48a1afe4f9c3d5c0ca450f12c9a605c20a0c3037c95",
            "f8ba5215b628deec40d735d6cdf825016fca443f0f8cbff3eb5361de59e8ab03",
        ),
    ];
    for (data_size, root, commitment) in rows {
        assert_root(Geometry::TESTING, *data_size, root, commitment);
    }
}

#[test]
fn mainnet_root_vectors() {
    let rows: &[(u64, &str, &str)] = &[
        (
            1,
            "2438dc9c444ab818a41197dfd3aa6502bc3e2fb03895b7ed79ab275539b7972b",
            "719863e12c1e714b1df22d0600ed93ad96486cbc7920f565a5e434574463d013",
        ),
        (
            4095,
            "851d7b0214e05f85e581de8dd408f5a4b0c31d8baad3f42986f056949ee1d575",
            "e773c7e0780a33fca2cbb91aae23fc0ba476ea86eb156ee79e93df35d1dda5be",
        ),
        (
            4096,
            "be58572dcfba33df3201cdbeda2799dd0c5b508a0df41a47912cb008896f9195",
            "be251ef52dcd4fd580211d8fb5eb451be18a3e02c16d4512b56953d9a3c1e748",
        ),
        (
            4097,
            "f516d84903ce88d9d1d80d4071ce945dc53707bd4ed60346a3ba6392412c9134",
            "ce4a935a089dd36beb84501e6813bf4e328bb0ed15307ac3c4a6ab246d04c3c6",
        ),
        (
            262_143,
            "b6dd65dde53181e3443d128d05b7cbf57c5f4352caa8216ed365d716896335bd",
            "7db8ca90c9fab87dc8577252f4ac084bfbc12f491ff049318352623ae715a83c",
        ),
        (
            262_144,
            "8e2d2d85eb5fd8e66d4f4b874b5ad6b5aa46cad790fbe64f61c374d8f8243d5f",
            "97c9ecd15caca57f3f74198a5a456d583fedd22ed267f951cce1cd14998b6e4f",
        ),
        (
            262_145,
            "dcc4ac982c84bdf0d6df5c16d2728b65bb4bae355b078b5a22e47a6a3e518825",
            "75f80cf9008e5f4c65a847f036fccd0c10176cc9840f82a4f0d6b846404d6326",
        ),
        (
            917_504,
            "3d86c811acf3356576cbe00b28814ad89ee42bd5aa36b755cea977620366de67",
            "472cfc752f2aacd4662235abe756da1e2079db705736ad22a6db9db6f6d6cc9c",
        ),
        (
            1_310_720,
            "1d5501730df0426a6f2b223d3fc31703d5eb6e64bae166736c920ccd23594ac0",
            "027fba531187d84813dd203d3e03dda3a3898cde1bbfd2675b54e46c230d6e89",
        ),
    ];
    for (data_size, root, commitment) in rows {
        assert_root(Geometry::MAINNET, *data_size, root, commitment);
    }
}

#[test]
fn testing_n112_proof_vectors() {
    let geometry = Geometry::TESTING;
    let data_size = 112_u64;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();

    let chunk2 = chunk_bytes(&geometry, &data, data_size, 2);
    let chunk_proof = tree.chunk_proof(2).unwrap();
    // spec §13: the reference printed the siblings. The last hash is the witness.
    let chunk_siblings = concat_hex(&[
        "dbc3c2433981a503f8ffa38629a9ec5d41501618b2afd860af52d6a93e06e8b6",
        "50cb073b3d437e037c34dc0d757489f56fd63b9122dd4edfbe8bc42812a5bd84",
    ]);
    assert_eq!(chunk_proof[..chunk_proof.len() - HASH_SIZE], chunk_siblings);
    assert_eq!(
        &chunk_proof[chunk_proof.len() - HASH_SIZE..],
        size_witness(data_size).as_bytes()
    );
    verify_chunk(&geometry, &commitment, data_size, 2, &chunk2, &chunk_proof).unwrap();

    let chunk3 = chunk_bytes(&geometry, &data, data_size, 3);
    let segment_proof = tree.segment_proof(3, 1, &chunk3).unwrap();
    // spec §13: the reference printed the siblings. The last hash is the witness.
    let segment_siblings = concat_hex(&[
        "82bdff7cd7950d248431b84dd588f1777777ee141ebc86ac8f72080aaf11e0e5",
        "fe43d66afa4a9a5c4f9c9da89f4ffb52635c8f342e7ffb731d68e36c5982072a",
        "f8e87ba8c906d8391fb1182e6c1e7d123392e8770d510b547edb3bbffe7cfb65",
        "50cb073b3d437e037c34dc0d757489f56fd63b9122dd4edfbe8bc42812a5bd84",
    ]);
    assert_eq!(
        segment_proof[..segment_proof.len() - HASH_SIZE],
        segment_siblings
    );
    assert_eq!(
        &segment_proof[segment_proof.len() - HASH_SIZE..],
        size_witness(data_size).as_bytes()
    );
    let segment_length = usize::try_from(geometry.segment_len(data_size, 3, 1).unwrap()).unwrap();
    let segment_offset = usize::try_from(geometry.segment_size()).unwrap();
    verify_segment(
        &geometry,
        &commitment,
        data_size,
        3,
        1,
        &chunk3[segment_offset..segment_offset + segment_length],
        &segment_proof,
    )
    .unwrap();

    assert_hex(
        tree.chunk_key(3).unwrap().as_bytes(),
        "b541f01b947453085184d77f79e23e48acd6d3f0ff773843392ab4dafad1b425",
    );
}

fn assert_root(geometry: Geometry, data_size: u64, root: &str, commitment: &str) {
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree =
        DataTree::from_bytes(geometry, &data).unwrap_or_else(|err| panic!("n={data_size}: {err}"));
    assert_eq!(
        u64::from(tree.chunk_count()),
        geometry.chunk_count(data_size).unwrap().into()
    );
    assert_hex(tree.data_tree_root().as_bytes(), root);
    assert_hex(tree.data_commitment().as_bytes(), commitment);
    assert_eq!(tree.size_witness(), hash::size_witness(data_size));
}

fn chunk_bytes(geometry: &Geometry, data: &[u8], data_size: u64, chunk_index: u32) -> Vec<u8> {
    let start =
        usize::try_from(chunk_index).unwrap() * usize::try_from(geometry.chunk_size()).unwrap();
    let chunk_length =
        usize::try_from(geometry.chunk_len(data_size, chunk_index).unwrap()).unwrap();
    data[start..start + chunk_length].to_vec()
}
