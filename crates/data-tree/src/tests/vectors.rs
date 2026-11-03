//! Checked-in hashes and roots from spec §10. The numbers in the rows are the
//! vector inputs; the hex strings are the expected digests.

use super::{assert_hex, concat_hex, empty_at_height, pattern};
use crate::MAX_SUBTREE_HEIGHT;
use crate::SubtreeHeight;
use crate::geometry::Geometry;
use crate::hash::{self, leaf, size_witness};
use crate::types::DataTreeRoot;
use crate::types::HASH_SIZE;
use crate::verify::{verify_chunk, verify_segment};
use crate::{DataTree, chunk_leaf, chunk_root};

#[test]
fn primitive_vectors() {
    assert_hex(
        leaf(b"").as_bytes(),
        "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
    );
    assert_eq!(empty_at_height(0), leaf(b""));
    assert_hex(
        empty_at_height(6).as_bytes(),
        "2be3cf0551cc6936d461e3dc43f3c4bf50cbee1bc091925254e879f4e7665e94",
    );
    for empty_height in 1..=MAX_SUBTREE_HEIGHT {
        assert_eq!(
            empty_at_height(empty_height),
            hash::node(
                &empty_at_height(empty_height - 1),
                &empty_at_height(empty_height - 1)
            )
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
fn subtree_height_stops_at_8() {
    assert_eq!(
        SubtreeHeight::new(MAX_SUBTREE_HEIGHT).map(SubtreeHeight::get),
        Some(MAX_SUBTREE_HEIGHT)
    );
    assert_eq!(SubtreeHeight::new(MAX_SUBTREE_HEIGHT + 1), None);
}

#[test]
fn testing_root_vectors() {
    let rows: &[(u64, &str, &str)] = &[
        (
            1,
            "0f5fe2ca48d3d3f37848fc851257fc094c8b27feaa82c905b2b8ffbd177f6543",
            "6766c8a6ed3afeaeb38d74b508b774ee33c8396d1061ac2bd272bedd02f5c421",
        ),
        (
            8,
            "bed46322520adad4b5c1808a4a30269e4fdbf89e89b83a1b8439442c01db2c83",
            "e35809d4fdba5e31164f9e58ac91d659a117776eb7748cd7e518239039a92f2b",
        ),
        (
            9,
            "ffa37bcad0b5968333e8f06b23b1f4cc430d4ae8a37a344de34ffc631dd8bf21",
            "515b428e3fa513840a5cf842a30d364a47d3cba26d7a483c845431d7ffebc852",
        ),
        (
            32,
            "5a3b3c37b21874d69166aa06c52a0b5dd87ec378187adab4bc9d4ee42eb0ef0d",
            "c21dea135c5673eb07cf2d641d7893b22ca83dd0986dc51b09b99b29026fdd6f",
        ),
        (
            33,
            "3bf5a0750be0980d8941fdf6778ba8b61b633599aee08a7d710d8566a4baa787",
            "c6ae46330fe254aebad232ff69bd954da161053bf629b8a64f130a4b2a4eb54d",
        ),
        (
            112,
            "0a22641c8d9f53afbe333e832a5187307f4d4f31a0b43dad7ea3dfc0f84deb6e",
            "82204503a718db465b3eeecea24b2896411211af0080555657eb2098fb969620",
        ),
        (
            157,
            "ca4c355dba86031d9f6eb8d8955305abf03cdc51de06ed264873a247148d64e1",
            "74e1e5bff4f5047875b49f5d20afc762335d3c07002b8e55acba6e65357ba20d",
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
            "646847d975529f524fd1f887a1dbdfb4514bece22e63e115d8a27b596bc5b827",
            "a345efb5e682e6b6d54bf57e9d5632976c1ccef897b0214a1c4c2b4cece7eb5a",
        ),
        (
            4095,
            "ead4d363fdb5a39a35e89c15b67574c9116018ad3f02264c1ab7a7248f44b632",
            "8994d60a594d001d6511afd273403fdc41c48e5e7b744a640ffab046737959b0",
        ),
        (
            4096,
            "b298a715260a196611a1a200298372756ad785dd459933d83ddfde245cd93bed",
            "46e6159e0bfad886ad65fd1a4c48ae7f93160944353e7d678a6d2a2f14178d30",
        ),
        (
            4097,
            "2a6d480a36e68670061d18861bb2c4308663b7c26963d4b13fbc5ccf42b463d2",
            "d57e36e3153ab9be17bd7cb1421909fbfe4360ee9d6d126d6a8705dcdf1e030f",
        ),
        (
            262_143,
            "32ca509db863c5bccc517b8f6e94927da71404e1023f89c93390ff58af9326da",
            "2bb9b4b7a3b6eeecb1e99eae46cd85efd969619f2d65e649628c10f4d53d544c",
        ),
        (
            262_144,
            "cc7b595dd3aecd15cfb5ce7a733f449e63c05545a8d5f9e8e67d5bf4b6069b0d",
            "9821bdbe69355f8a624164e5d73fb676e2cfb7af7483327cf6fc922c0b873b39",
        ),
        (
            262_145,
            "5504c86e3cf316bf05ecd96a2585c3b09781382c25f6c664b5330a5a444c3101",
            "d9243949c7c500fa0606a8a7d4ad4285c727e81944f42d283f49f0d0544e770c",
        ),
        (
            917_504,
            "7f4f92981ff51822adf2c7f7a394a9536e764256c8bd8b4d5daef31a62b8de07",
            "0e751421c6429cba0e9ac6edc8c32edb88006e0d052518f6bd933d4b2a000d5e",
        ),
        (
            1_310_720,
            "29f12d7ecbcface82c0ca4787c60bd6d6a0fb271a173599c977eee68aff59731",
            "bac42e0026f41eff11d056d4ee30904dcf963734eef88c310c9a950f72ab0069",
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
    // spec §10: the generator printed the siblings. The last hash is the witness.
    let chunk_siblings = concat_hex(&[
        "ec954565828ae61fff557dcb9cc3b4e6a5da4b5417db597bffc7e9d6ac716332",
        "4bde04027ed46b4dc676b850d95e5b4f8ab4092f4c374528905967fbc157e399",
    ]);
    assert_eq!(chunk_proof[..chunk_proof.len() - HASH_SIZE], chunk_siblings);
    assert_eq!(
        &chunk_proof[chunk_proof.len() - HASH_SIZE..],
        size_witness(&geometry, data_size).as_bytes()
    );
    verify_chunk(&geometry, &commitment, data_size, 2, &chunk2, &chunk_proof).unwrap();

    let chunk3 = chunk_bytes(&geometry, &data, data_size, 3);
    let segment_proof = tree.segment_proof(3, 1, &chunk3).unwrap();
    // spec §10: the generator printed the siblings. The last hash is the witness.
    let segment_siblings = concat_hex(&[
        "82bdff7cd7950d248431b84dd588f1777777ee141ebc86ac8f72080aaf11e0e5",
        "fe43d66afa4a9a5c4f9c9da89f4ffb52635c8f342e7ffb731d68e36c5982072a",
        "fbc10578ab6a756a167ab57c071797202dd7a56d16acca9b268c230c87b0a7cc",
        "4bde04027ed46b4dc676b850d95e5b4f8ab4092f4c374528905967fbc157e399",
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
        "507655a3cf308a380bd30de15bade83e504e6271cbba0482de73d5e58fe37bbc",
    );
}

/// spec §10 printed chunk leaves. For `C = 1` the root is `chunk_leaf(0, R_0)` (spec §4).
#[test]
fn testing_chunk_leaf_vectors() {
    let geometry = Geometry::TESTING;
    for (data_size, chunk_leaf_hex) in [
        (
            1_u64,
            "0f5fe2ca48d3d3f37848fc851257fc094c8b27feaa82c905b2b8ffbd177f6543",
        ),
        (
            32,
            "5a3b3c37b21874d69166aa06c52a0b5dd87ec378187adab4bc9d4ee42eb0ef0d",
        ),
    ] {
        let data = pattern(usize::try_from(data_size).unwrap());
        let tree = DataTree::from_bytes(geometry, &data).unwrap();
        let root = chunk_root(&geometry, &data).unwrap();
        assert_hex(chunk_leaf(0, &root).as_bytes(), chunk_leaf_hex);
        assert_hex(tree.data_tree_root().as_bytes(), chunk_leaf_hex);
        assert_hex(tree.chunk_leaf(0).unwrap().as_bytes(), chunk_leaf_hex);
    }

    let data_size = 112_u64;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let chunk3 = chunk_bytes(&geometry, &data, data_size, 3);
    let root = chunk_root(&geometry, &chunk3).unwrap();
    assert_hex(
        root.as_bytes(),
        "dbc3c2433981a503f8ffa38629a9ec5d41501618b2afd860af52d6a93e06e8b6",
    );
    let printed_leaf = "ec954565828ae61fff557dcb9cc3b4e6a5da4b5417db597bffc7e9d6ac716332";
    assert_hex(chunk_leaf(3, &root).as_bytes(), printed_leaf);
    assert_hex(tree.chunk_leaf(3).unwrap().as_bytes(), printed_leaf);
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
