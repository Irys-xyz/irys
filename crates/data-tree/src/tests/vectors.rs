//! Checked-in hashes and roots from spec §10. The numbers in the rows are the
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
    let mainnet = Geometry::MAINNET;
    assert_hex(
        hash::size_witness(&mainnet, 1).as_bytes(),
        "e677df7f858cd910622cfd5c6ac7bbf6200e7829c3e4a7325590a6b6309c0597",
    );
    assert_hex(
        hash::size_witness(&mainnet, 4096).as_bytes(),
        "c0c06f04b271e626a424dad90c9ad30189e210d4b9690f231d9147cdeca448f0",
    );
    assert_hex(
        hash::size_witness(&mainnet, 262_144).as_bytes(),
        "86b2be42530761806437d0632cfb93c56f9294c8cc1c8776ad01d08b3078fa49",
    );
    assert_hex(
        hash::size_witness(&mainnet, 917_504).as_bytes(),
        "438a131aaf3445c16ab8b8f66374f8ec253f8bce7acc9efd91f722e2409ef89e",
    );

    // spec §4: the commitment is node(data_tree_root, size_witness(G, n)).
    // Any filler byte. The check is the commitment formula, not this value.
    let root = DataTreeRoot::from([9_u8; HASH_SIZE]);
    let data_size = 123_u64;
    let via_node = hash::node(
        &crate::types::NodeHash::from(*root.as_bytes()),
        &crate::types::NodeHash::from(*hash::size_witness(&mainnet, data_size).as_bytes()),
    );
    assert_eq!(
        hash::data_commitment(&mainnet, &root, data_size).as_bytes(),
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
            "594c8b3b9cff69e0b5dd50c7465a7b2f7dc722d445f0ac2952d135c04b79f2b6",
        ),
        (
            8,
            "2239889b93ac82ead2db9605f2584a25255c90206c1fccaddf54caa75b7fc1a1",
            "6e9c94693cb2fea535b79bb9938982db61ff23ade48119a60340e9a680423469",
        ),
        (
            9,
            "3ba2878285288e004dcd35a73e9a3df4a19f5fe5acccaf8d84b50fc7254e6ddb",
            "245a5af1aa8e606cb4e8878d93449d7872f68de9bb8b1db696c13ea81c393728",
        ),
        (
            32,
            "d3e81f198d6bd3782d1941172c89e49721d8f5e33cc709d4f6ba3f54b9e35f82",
            "aa774e6e5029925d0c43c6f019c56222cb4f220bad6626bb37bdf17180f5d6ea",
        ),
        (
            33,
            "bf98e27c471bfda3de1d44b4eb420f528238aa75e6e002fe1636c04a431713a9",
            "074db04f4bc2e5cf84f47d2772cf100c0c09d372e15c28264f9edf9187645d00",
        ),
        (
            112,
            "7fd698c547deb0bbb8f8ab7db08dd34616a496f141167252487509cb74824564",
            "d3175b025ae584b19f464af849bff465ecf9b9a2c83376b947b5681d82cca1c4",
        ),
        (
            157,
            "81e7a996cd2ba141815aa48a1afe4f9c3d5c0ca450f12c9a605c20a0c3037c95",
            "8eb9e426d13ab02def39117e53b838110f8cebfdfb7d0b9c1726e05001c00e0d",
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
            "9029711cdb35496538bd4e91ac809a8abe2e3bb94376d44595e097478a4a8698",
        ),
        (
            4095,
            "851d7b0214e05f85e581de8dd408f5a4b0c31d8baad3f42986f056949ee1d575",
            "267ff99c8c7c3580124a730ee7e7c28781d3969a5e52ea164a8fc5fcfa78416b",
        ),
        (
            4096,
            "be58572dcfba33df3201cdbeda2799dd0c5b508a0df41a47912cb008896f9195",
            "7f3301e487d66e71bbb39ab1ae67d6c3ce37fee25eab9daf8259a3713cd4d47c",
        ),
        (
            4097,
            "f516d84903ce88d9d1d80d4071ce945dc53707bd4ed60346a3ba6392412c9134",
            "3262c546bc4db11e05171d083d33fef98a36ddbb18f56ee64fcc2b5f3bb0aade",
        ),
        (
            262_143,
            "b6dd65dde53181e3443d128d05b7cbf57c5f4352caa8216ed365d716896335bd",
            "5ab3d70ba6f6ef25d19d456d9d4bfa57ed26796d10339b4ba95f34bfd41d0f83",
        ),
        (
            262_144,
            "8e2d2d85eb5fd8e66d4f4b874b5ad6b5aa46cad790fbe64f61c374d8f8243d5f",
            "774bddbe62aae051508341d9291b1533eda68d8f95f034c6905de097aa185730",
        ),
        (
            262_145,
            "dcc4ac982c84bdf0d6df5c16d2728b65bb4bae355b078b5a22e47a6a3e518825",
            "ff03453747ac366f182de34614fef6816feda7428fc7d5b00f931561eaa8fc04",
        ),
        (
            917_504,
            "3d86c811acf3356576cbe00b28814ad89ee42bd5aa36b755cea977620366de67",
            "683b214b4775ab4fbe5baa64cf193f50a1ecb06ea3a90f3b3e85a60639c3c91f",
        ),
        (
            1_310_720,
            "1d5501730df0426a6f2b223d3fc31703d5eb6e64bae166736c920ccd23594ac0",
            "36c380c4cb361fe958b443d27656130e5f57e58b9fd8c80af9a0130a390b83ab",
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
    // spec §10: the reference printed the siblings. The last hash is the witness.
    let chunk_siblings = concat_hex(&[
        "dbc3c2433981a503f8ffa38629a9ec5d41501618b2afd860af52d6a93e06e8b6",
        "50cb073b3d437e037c34dc0d757489f56fd63b9122dd4edfbe8bc42812a5bd84",
    ]);
    assert_eq!(chunk_proof[..chunk_proof.len() - HASH_SIZE], chunk_siblings);
    assert_eq!(
        &chunk_proof[chunk_proof.len() - HASH_SIZE..],
        size_witness(&geometry, data_size).as_bytes()
    );
    verify_chunk(&geometry, &commitment, data_size, 2, &chunk2, &chunk_proof).unwrap();

    let chunk3 = chunk_bytes(&geometry, &data, data_size, 3);
    let segment_proof = tree.segment_proof(3, 1, &chunk3).unwrap();
    // spec §10: the reference printed the siblings. The last hash is the witness.
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
        size_witness(&geometry, data_size).as_bytes()
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
        "91ff543f909c38411b53d65ef72e411ed81721af90f8f88f6b49450688b32674",
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
    assert_eq!(
        tree.size_witness(),
        hash::size_witness(&geometry, data_size)
    );
}

fn chunk_bytes(geometry: &Geometry, data: &[u8], data_size: u64, chunk_index: u32) -> Vec<u8> {
    let start =
        usize::try_from(chunk_index).unwrap() * usize::try_from(geometry.chunk_size()).unwrap();
    let chunk_length =
        usize::try_from(geometry.chunk_len(data_size, chunk_index).unwrap()).unwrap();
    data[start..start + chunk_length].to_vec()
}
