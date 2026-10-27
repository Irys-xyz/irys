<!-- MDBX-SUBMODULE-INDEX: delete this page when the MDBX submodule index and these markers are gone. The consensus database stays MDBX. -->

# Remove the MDBX storage-module index

This page lists code that exists because some nodes still run the MDBX per-submodule index (`db/`, schema v2 marker `irys-submodule-index 2`). RocksDB v1 lives in `rocks/` (`irys-submodule-index rocksdb 1`). The consensus database stays MDBX. Do not use these markers on consensus open, the cache database, the block index, or reth.

Search for the marker prefix in the HTML comment at the top of this page. Each hit in a code comment, a TOML comment, or an HTML comment is one removal site. Line numbers locate each marker in the current source. The marker text identifies the site after a line moves.

## Preconditions

1. Every node runs RocksDB v1. `database.submodule_index` is `rocksdb`. The live index directory is `rocks/`.
2. The rollback window is closed. No operator still needs `rocks-to-mdbx`, and no live `db/` must stay readable.
3. The ansible switch plays and rollback plays are retired.
4. `DatabaseConfig` denies unknown fields. Keep parsing `submodule_index` until every config file drops the key. Delete the field after that. An omitted key selects MDBX until then.

## Removal order

1. Meet the preconditions.
2. Retire the ansible plays in the Ansible section, including the canary and flip guards that exist for this migration. Delete the `.irys-db-mdbx-mtime` stamp with those plays.
3. Delete the converters and the MDBX verify: the MDBX functions in `convert.rs`, the `submodule-index-convert` commands `mdbx-to-rocks`, `rocks-to-mdbx`, `verify`, and `--rollback`, and `SUBMODULE_CONVERT_INCOMPLETE`. `verify_rocks_records` and the `verify-rocks` command compare two RocksDB v1 directories. Move them out if that compare stays.
4. Delete the v1 to v2 migration: `migrate.rs`, `verify.rs`, `tables::v1`, `tables::v1_fixture`, the `submodule-index-migrate` binary, and `--migrate-index` on the derive bench.
5. Make RocksDB the only open path in `open_submodule_index`. Keep parsing `submodule_index` until step 4 of the preconditions is done.
6. Delete `MdbxSubmoduleStore`, `db.rs`, the `tables!` MDBX table set, `submodule_db_args`, and `MDBX_SAFE_NOSYNC` on the submodule index. Move the placement-extent and span-overlap helpers out of `db.rs` first. `OffsetMeta`, `PackedOffsetRow`, and `Placement` stay. `SUBMODULE_SCHEMA_ROCKS_V1` stays.
7. Collapse `SubmoduleIndex`, `dispatch_index!`, and the `SubmoduleRead` / `SubmoduleWrite` / `SubmoduleStore` traits onto `RocksSubmoduleStore`. Keep the behavior RocksDB v1 implements. Delete the trait default methods. Those defaults are the MDBX behavior.
8. Delete `stores_tx_paths()` true arms and `stores_offset_crc()` false arms. Delete `TxPathSource::Stored(Some)` (the MDBX data-chunk path). `Stored(None)` stays: capacity chunks on RocksDB still use it. The derive path stays.
9. Delete MDBX-only tests, the MDBX case of dual-engine tests, the MDBX half of `submodule-index-bench`, and the `irys-sm-bench` MDBX default. Retarget helpers that open MDBX only so the engine-agnostic checks still run on RocksDB.

## Delete together

- Migration: `migrate.rs`, `verify.rs`, `tables::v1`, `tables::v1_fixture`, `submodule-index-migrate`, `--migrate-index`.
- Convert: MDBX functions in `convert.rs`, `submodule-index-convert` except `verify-rocks`, `SUBMODULE_CONVERT_INCOMPLETE`.
- Engine: `MdbxSubmoduleStore`, `db.rs` (after the helper move), the `tables!` set, `submodule_db_args`, submodule `MDBX_SAFE_NOSYNC`.
- Config: `SubmoduleIndexEngine` and the `submodule_index` field, after every config drops the key. Then the Mdbx arm of `open_submodule_index`.
- Traits last. Callers go through `SubmoduleIndex` until step 7.

Production enables group commit only when the engine is RocksDB. `MDBX_SAFE_NOSYNC` on the submodule index aborts inside libmdbx, so the MDBX engine stays on one durable commit per registration. After step 6, delete that engine check and always enable group commit. `group.rs` stays. The `true` result of `note_visible` is the MDBX sync trigger. RocksDB ignores the bool.

## Stays unmarked

- Consensus MDBX: `consensus_db_args`, `open_or_create_db`, the cache database, the block index.
- `SUBMODULE_SCHEMA_ROCKS_V1`.
- `verify_rocks_records` and CLI `verify-rocks`.
- `TxPathSource::Stored(None)` and the tx-path derive.
- `index_wal_backstop_ms` (RocksDB WAL backstop).
- RocksDB arms where `stores_offset_crc()` is true.
- `group.rs`, except the `note_visible` marker.

## Ansible

The plays live in the ansible repo. This tree does not contain them. Retire them in step 2. Do not rename `.irys-db-mdbx-mtime` on its own. Delete it with the plays that write it (`shared/tasks/submodule_index_convert_one.yml`, `shared/tasks/submodule_index_reverify_if_moved.sh`).

Switch play: `shared/submodule_index_rocksdb.yml`, play name "Convert submodule indexes to RocksDB v1 and switch the engine".

Rollback play: `shared/submodule_index_rollback.yml`, play name "Roll back the RocksDB submodule index switch". It installs the binary the switch kept at `target/<build_profile>/irys.pre-rocks`.

Wrappers, each an import of the shared play:

- `devnet/submodule_index_rocksdb.yml`, `testnet/submodule_index_rocksdb.yml`, `mainnet/submodule_index_rocksdb.yml`
- `devnet/submodule_index_rollback.yml`, `testnet/submodule_index_rollback.yml`, `mainnet/submodule_index_rollback.yml`

Tasks:

- `shared/tasks/submodule_index_classify_one.yml`
- `shared/tasks/submodule_index_convert_one.yml`
- `shared/tasks/submodule_index_recheck_stopped.yml`
- `shared/tasks/submodule_index_disk_room.sh`
- `shared/tasks/submodule_index_require_kill_timeout.sh`
- `shared/tasks/submodule_index_retire_rocks.sh`
- `shared/tasks/submodule_index_require_v1_db.sh`
- `shared/tasks/submodule_index_reverify_if_moved.sh`

Test: `tests/submodule_index_rocksdb_test.py`.

Guards outside those plays:

- `shared/tasks/flip_and_restart.yml` refuses a RocksDB binary when `submodule_index` is unset, and refuses a binary that cannot parse the key.
- `shared/canary_update.yml` refuses a torn binary even when the config has no `submodule_index` key, and greps `^submodule_index`.
- The switch writes `submodule_index_engine: rocksdb` into `devnet/host_vars/devnet-node-1.yml` through `devnet-node-4.yml`. Rollback removes that key.

## Marker index

### irys-database

#### `crates/database/Cargo.toml`

- L42: delete the MDBX engine path of this bench. The RocksDB timings can stay.
- L48: remove this bin with the v1 to v2 MDBX migration.
- L53: remove this bin once the rollback window is closed.

#### `crates/database/src/bin/submodule_index_bench.rs`

- L1: delete the MDBX engine path in this bench. The RocksDB timings can stay.
- L47: delete this constant and MID_MDBX_PATH_BATCH with the MDBX bench path.
- L51: delete this constant with the MDBX bench path.
- L154: delete this variant. Keep Rocks.
- L168: delete this method with the MDBX bench path.
- L291: delete this block. It times the MDBX engine.
- L998: delete this arm. Only MDBX stores the writer hash.
- L1163: delete this arm. Only MDBX stores the tx-path hash.
- L1196: make this the only path. The append below is the MDBX write.
- L2630: delete this refusal with the MDBX bench path. Checksums run on RocksDB only.

#### `crates/database/src/bin/submodule_index_convert.rs`

- L1: remove this binary once the rollback window is closed. The verify-rocks command compares two RocksDB v1 directories and can be split out if that compare is still wanted.
- L251: delete this command.
- L253: delete this command once the rollback window is closed.
- L255: delete this command. It compares an MDBX index with RocksDB v1. Keep verify-rocks if a Rocks-only compare is still wanted.
- L294: delete this flag with verify of a rocks-to-mdbx copy.

#### `crates/database/src/bin/submodule_index_migrate.rs`

- L1: remove this binary with the v1 to v2 MDBX migration.

#### `crates/database/src/bin/tx_path_derive_bench.rs`

- L105: delete this variant. The derive bench stays for RocksDB.
- L115: delete this variant with baseline-stored. That mode times a tx path only MDBX stores.
- L135: delete this flag with the v1 to v2 MDBX migration. The derive bench itself stays for RocksDB.
- L274: delete these Baseline arms with baseline-stored.
- L455: delete this mode. It times a stored tx path, which only MDBX keeps.
- L949: delete this helper with the MDBX bench path. RocksDB open does not use it.
- L957: delete this helper with the MDBX bench path.
- L964: delete this enum with the MDBX bench path.
- L973: delete this helper with the MDBX bench path.
- L983: delete this helper with the MDBX bench path.
- L1011: delete this arm.
- L1017: delete this helper with the MDBX bench path.
- L1039: delete this helper with the MDBX bench path.
- L1086: delete this arm.
- L1108: delete this helper with the MDBX bench path.
- L1125: delete this helper with the MDBX bench path.
- L1155: delete this helper with the MDBX bench path.
- L1218: delete this arm. Auto-detect of mdbx.dat goes away with the MDBX index.
- L1237: delete this arm.
- L1364: delete this helper with --migrate-index. It guards the v1 to v2 MDBX migration.
- L1383: delete this fixture writer with the MDBX baseline. It stores tx paths in an MDBX index.
- L1498: delete this helper with the MDBX baseline. It writes stored tx paths.
- L1852: delete this flag with the v1 to v2 MDBX migration.
- L1881: delete this arm.
- L1893: delete this arm with baseline-stored.

#### `crates/database/src/database.rs`

- L488: delete this function with the MDBX submodule engine. Consensus keeps consensus_db_args.

#### `crates/database/src/submodule/convert.rs`

- L1: delete the MDBX conversion and the MDBX verify in this file once every node is on RocksDB v1 and the rollback window is closed. verify_rocks_records compares two RocksDB v1 directories and is not an MDBX path.
- L281: delete this flag with --rollback. It accepts a rocks-to-mdbx copy that dropped checksums.
- L523: delete this function with the mdbx-to-rocks command.
- L740: delete this function with the rocks-to-mdbx command once the rollback window is closed.
- L797: delete this function with verify of an MDBX source. verify_rocks_records is the Rocks-only compare.
- L2583: delete this helper with the MDBX source reader.

#### `crates/database/src/submodule/db.rs`

- L1: delete this MDBX table API with MdbxSubmoduleStore. Move engine-agnostic helpers (placement extent, span overlap) out before deleting the file. The consensus database is a different MDBX environment and stays.
- L64: delete this open with the MDBX engine. It migrates a schema v1 directory in place before it returns.

#### `crates/database/src/submodule/group.rs`

- L243: the true result is the MDBX sync trigger in MdbxSubmoduleStore::update_registration. RocksDB ignores the bool. Delete that caller with the MDBX store.

#### `crates/database/src/submodule/migrate.rs`

- L1: remove this file with the submodule-index-migrate CLI. It upgrades an MDBX schema v1 submodule index to schema v2 in place.
- L540: delete with rocks-to-mdbx. Open refuses an MDBX directory left unfinished by that conversion.

#### `crates/database/src/submodule/mod.rs`

- L2: delete this module with the converters once the rollback window is closed.
- L4: delete this module with the MDBX submodule engine. The consensus database stays MDBX.
- L8: remove with the migrate CLI. This is the v1 to v2 in-place MDBX migration.
- L18: remove with the migrate CLI. This compares MDBX schema v1 with the migrated schema v2.
- L22: drop convert_mdbx_to_rocks, convert_rocks_to_mdbx, and verify_mdbx_and_rocks from this export. verify_rocks_records can move out.
- L27: delete this re-export with db.rs.
- L41: drop MdbxSubmoduleStore from this export when the MDBX engine is deleted. The other names stay until the trait collapses onto RocksSubmoduleStore.
- L47: remove this re-export with the migrate CLI.

#### `crates/database/src/submodule/store.rs`

- L53: collapse this trait onto RocksSubmoduleStore when MDBX is gone. Do not delete behavior RocksDB v1 implements. The default methods below are the MDBX behavior.
- L91: delete this default. MDBX stores tx-path bytes. RocksDB v1 returns false and derives them.
- L223: delete this default. MDBX stores no checksum. RocksDB v1 returns true.
- L232: delete this default. It is the MDBX proof with no checksum. RocksDB v1 reads the offset row.
- L246: delete this default. MDBX does not scan checksum rows. RocksDB v1 returns them.
- L259: delete this default. MDBX reports every span incomplete. RocksDB v1 scans stored checksums.
- L270: collapse this trait onto RocksSubmoduleStore with SubmoduleRead. The default methods below are the MDBX behavior.
- L347: delete this default. MDBX appends the info. RocksDB v1 updates the one interval.
- L359: delete this default. MDBX returns false. RocksDB v1 stores the data-root row on the interval.
- L407: delete this default. MDBX ignores ledger, height, and tx index. RocksDB v1 copies them onto the placement.
- L422: delete this default. MDBX ignores crc and writes the proof. RocksDB v1 stores the checksum.
- L460: collapse this trait onto RocksSubmoduleStore. It exists so MDBX and RocksDB share one open path.
- L519: delete this default. MDBX stores no checksum. RocksDB v1 returns true.
- L525: delete this default. MDBX stays false. RocksDB v1 reads the metadata flag.
- L531: delete this default. MDBX ignores the flag. RocksDB v1 writes it.
- L540: delete this default. MDBX has no checksum row to keep, so it drops every presence row. RocksDB v1 keeps the checksum.
- L551: delete this type and its impls. It is the MDBX submodule index.
- L756: delete with MdbxSubmoduleStore. This arms MDBX_SAFE_NOSYNC for the submodule index. RocksDB group commit is a different method.
- L817: delete with MdbxSubmoduleStore. This sets MDBX_SAFE_NOSYNC on the submodule environment.
- L895: delete with MdbxSubmoduleStore. This sets MDBX_SAFE_NOSYNC on the submodule index only.
- L1347: make RocksSubmoduleStore the only type and delete this enum.
- L1349: delete this variant and every Self::Mdbx match arm. Callers use RocksSubmoduleStore.
- L1356: delete this macro with the SubmoduleIndex enum.
- L1368: delete this constructor with the MDBX engine.
- L1397: delete this arm. MDBX stores tx paths. RocksDB v1 does not.
- L1415: delete this arm. MDBX ignores the packing record. RocksDB v1 binds it.
- L1596: delete this arm. MDBX ignores the WAL backstop.
- L1708: delete this test hook with MDBX_SAFE_NOSYNC.
- L1852: delete this arm. It checks tx-path bytes only MDBX stores.
- L1869: delete this arm. It checks tx-path bytes only MDBX stores.
- L1898: delete the MDBX open in this helper. Keep the RocksDB call.
- L1960: delete this test. The body runs only when the index stores tx paths, which is MDBX.
- L2125: make this the only path. RocksDB v1 does not store tx paths.
- L2172: delete this arm. It checks tx-path bytes only MDBX stores.
- L2409: delete this arm. It checks tx-path bytes only MDBX stores.
- L2463: delete this arm. It checks tx-path bytes only MDBX stores.
- L2493: make this the only path. RocksDB v1 does not store tx paths.
- L3316: delete this arm. It checks tx-path bytes only MDBX stores.
- L3391: delete this test with MDBX group commit. RocksDB does not sync on the registration count.
- L3540: delete this arm. It checks tx-path bytes only MDBX stores.
- L3563: delete this arm. It checks tx-path bytes only MDBX stores.
- L4024: delete this test with MDBX_SAFE_NOSYNC.

#### `crates/database/src/submodule/tables.rs`

- L18: delete this MDBX schema v2 marker with the MDBX engine. RocksDB v1 uses SUBMODULE_SCHEMA_ROCKS_V1.
- L57: remove with rocks-to-mdbx. Open refuses a directory that still has this row.
- L61: delete this MDBX table set with the MDBX engine. OffsetMeta, PackedOffsetRow, and Placement below are shared with RocksDB v1 and stay.
- L253: remove with the v1 to v2 migration. This is the schema v1 data-path table.
- L296: delete this test with the MDBX table set.
- L359: remove with the v1 to v2 migration. Test writers use this to build a release-4.0.8 db/.

#### `crates/database/src/submodule/verify.rs`

- L1: remove this file with the migrate CLI. It compares a pristine MDBX schema v1 index with the migrated schema v2 index.

#### `crates/database/src/tx_path_derive.rs`

- L1504: delete this test. It stores a tx path in an MDBX submodule index. RocksDB v1 derives the path and does not store it.
- L1646: delete this test. It stores a tx path in an MDBX submodule index. The derive against the consensus database stays.

### irys-types

#### `crates/types/src/block_production.rs`

- L50: Stored(Some) is the MDBX data-chunk path; delete that use. Stored(None) stays: capacity chunks on RocksDB still use it.

#### `crates/types/src/config/node.rs`

- L60: make RocksDB the only submodule index engine, then remove this enum and the submodule_index key. The consensus database stays MDBX. DatabaseConfig denies unknown fields, so delete the key only after configs no longer contain it.
- L126: remove this field once every config is on RocksDB and the key has been retired. An omitted key selects MDBX.
- L187: delete this assignment with the field. The default is MDBX until then.
- L1873: point test configs at RocksDB when that is the only engine. This line selects MDBX.
- L2304: delete the submodule_index assertion in this test when the field is removed. The other assertions stay.
- L2360: rewrite this test when the key is removed. It pins an omitted key to MDBX.

### irys-domain

#### `crates/domain/src/models/chunk_crc.rs`

- L639: delete this test with the MDBX engine. It asserts that MDBX stores no checksum.

#### `crates/domain/src/models/chunk_provider.rs`

- L720: this test opens only MDBX. Retarget it at RocksDB.
- L745: this test opens only MDBX. Retarget it at RocksDB.
- L781: delete this MDBX case and keep the RocksDB case.
- L826: delete this MDBX case and keep the RocksDB case.
- L870: delete this MDBX case and keep the RocksDB case.
- L914: this test opens only MDBX. Retarget it at RocksDB.

#### `crates/domain/src/models/storage_module.rs`

- L188: delete the else. MDBX has no checksum on the offset.
- L199: delete the else. It is the MDBX proof read. RocksDB reads the proof from the offset row.
- L289: delete this arm. Only MDBX stores tx-path bytes. RocksDB v1 derives them.
- L1567: delete the Mdbx arm. Open rocks/ only.
- L1573: delete this arm.
- L1580: delete this arm. It opens db/ and migrates schema v1 in place.
- L1614: delete this refusal once db/ is no longer a live index and the rollback window is closed.
- L1633: delete this log with the kept db/ once rollback is retired.
- L1661: delete the Mdbx variant. The bench opens rocks/ only.
- L1761: delete this helper with the MDBX bench open.
- L2516: delete this engine check and always enable group commit. MDBX is left off because its SAFE_NOSYNC path aborts inside libmdbx.
- L2641: delete this MDBX bench open.
- L3441: delete this arm. MDBX treats the interval label as presence because it stores no checksum row.
- L4640: delete this arm. MDBX stores proofs only and drops an entropy path.
- L5876: delete this else arm. MDBX drops every non-Data presence row. RocksDB v1 keeps an entropy checksum.
- L6404: delete this arm. MDBX has no checksum to reject.
- L6506: delete the else. MDBX has no checksum, so the value is None.
- L6521: delete this arm. MDBX does not confirm a checksum.
- L6742: delete this arm. MDBX returns Skip because it stores no checksum.
- L6937: delete this return. It skips the checksum scan when every submodule index is MDBX.
- L6962: delete this continue. An MDBX index stores no checksum.
- L7013: delete this continue. An MDBX index stores no checksum.
- L7130: delete this continue. An MDBX index stores no checksum.
- L10553: retarget this bench test at BenchIndex::Rocks. It opens MDBX.
- L10591: delete this MDBX bench open.
- L10616: retarget this helper at BenchIndex::Rocks, or delete it with the MDBX bench open.
- L10655: delete this MDBX bench open.
- L10874: delete this arm with the MDBX open path.
- L10884: delete this arm with the MDBX open path.
- L10959: delete this schema read with the MDBX open path.
- L11207: delete this test with the MDBX open path.
- L11327: delete this test with the MDBX open path.
- L11360: delete this helper with the MDBX open tests.
- L11372: delete this helper with the MDBX open tests.
- L11416: delete this test with the MDBX open path.
- L11528: delete the MDBX half of this test. The RocksDB group-commit assertion stays.
- L11821: drop Mdbx from this list and keep the RocksDB run.
- L11959: delete the stored_paths arms in this helper. They check the tx path only MDBX keeps.
- L12386: drop Mdbx from this list and keep the RocksDB run.
- L12808: drop Mdbx from this list and keep the RocksDB run.
- L12889: drop Mdbx from this list and keep the RocksDB run.
- L12970: drop Mdbx from this list and keep the RocksDB run.
- L13099: drop Mdbx from this list and keep the RocksDB run.
- L13194: drop Mdbx from this list and keep the RocksDB run.
- L13466: delete this MDBX case and keep the RocksDB case.
- L13529: delete this MDBX case and keep the RocksDB case.
- L13594: delete this MDBX case and keep the RocksDB case.
- L13651: delete this MDBX case and keep the RocksDB case.
- L13712: this test opens only MDBX. Retarget it at RocksDB. The interval edit is not MDBX-specific.
- L14100: delete this MDBX bench open.
- L17821: delete this MDBX case and keep the RocksDB case.
- L17866: delete this MDBX case and keep the RocksDB case.
- L19196: drop Mdbx from this list and keep the RocksDB run.
- L20696: drop Mdbx from this list and keep the RocksDB run.
- L21203: drop Mdbx from this list and keep the RocksDB run.
- L21288: drop Mdbx from this list and keep the RocksDB run.
- L21363: drop Mdbx from this list and keep the RocksDB run.
- L21465: drop Mdbx from this list and keep the RocksDB run.
- L21527: drop Mdbx from this list and keep the RocksDB run.
- L22008: drop Mdbx from this list and keep the RocksDB run.
- L22102: this test opens only MDBX. Retarget it at RocksDB.
- L22671: delete this MDBX case and keep the RocksDB case.
- L22775: delete this test. It checks the tx path MDBX commits with the bytes.

### irys-actors

#### `crates/actors/src/block_producer.rs`

- L2320: delete this test with Stored(Some). Capacity still uses Stored(None), which carries no bytes.

#### `crates/actors/src/chunk_migration_service.rs`

- L1123: this helper opens only MDBX. Retarget it at RocksDB, or delete the tests that call fixture.
- L1855: retarget this test at RocksDB. The scan bound is not MDBX-specific.
- L1972: retarget this test at RocksDB. The scan bound is not MDBX-specific.

#### `crates/actors/src/partition_mining_service.rs`

- L86: delete this variant. MDBX stores no placement identity on the derive path. RocksDB v1 checks the stored row.
- L628: delete this arm. Only MDBX returns a stored data-chunk tx path. RocksDB v1 derives it.
- L720: delete this arm. MDBX stores the tx path and skips the placement check.
- L2081: this helper opens only MDBX. Retarget it at RocksDB, or delete the tests that call open_data_miner.

### irys-sm-bench

#### `crates/tooling/sm-bench/src/drivers.rs`

- L1580: retarget this open at BenchIndex::Rocks. The check is not MDBX-specific.

#### `crates/tooling/sm-bench/src/main.rs`

- L107: change this default to Rocks when MDBX is gone, then remove the flag.
- L241: delete the Mdbx variant and make Rocks the only bench engine.
- L264: delete this arm.
- L446: delete this arm.
- L1311: delete this test when the bench default is Rocks.

#### `crates/tooling/sm-bench/src/module.rs`

- L302: retarget this open at BenchIndex::Rocks. The check is not MDBX-specific.
- L434: retarget this open at BenchIndex::Rocks. The check is not MDBX-specific.

#### `crates/tooling/sm-bench/src/prepop.rs`

- L251: this helper opens only MDBX. Retarget it at BenchIndex::Rocks.

### docs

#### `docs/60-indexes/06-storage-module-indexes.md`

- L3: delete the MDBX sections of this page once RocksDB v1 is the only submodule index. The consensus database stays MDBX.

#### `docs/60-indexes/mdbx-submodule-index-removal.md`

- L1: delete this page when the MDBX submodule index and these markers are gone. The consensus database stays MDBX.
