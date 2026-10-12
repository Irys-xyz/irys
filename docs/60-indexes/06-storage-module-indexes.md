# Storage module indexes

Each submodule keeps its index in `db/` (MDBX) or `rocks/` (RocksDB).
`database.submodule_index` selects the engine. The value is `mdbx` or
`rocksdb`. The node opens only that directory. It refuses a layout that
does not match the config. The node refuses to open MDBX when `rocks/`
also exists, because after a conversion `rocks/` is the live index and
`db/` stays beside it unread. A rollback to MDBX moves `rocks/` out of
the module directory (for example, rename it to `rocks.retired`) before
the node starts.

MDBX stays on schema v2. A fresh RocksDB index is schema 3.2
(`irys-submodule-index 3.2`). A RocksDB schema v2 directory still opens
until it is converted. Schema 3.2 stores the placement, the offset row
in `ChunkMetaByOffset`, and `ChunkOffsetsByContentHash` (the 32-byte
leaf hash, then the partition offset, with an empty value). The offset
row is the data proof when one is stored, and the CRC32C of the packed
chunk when one was written. A checksum with no proof is an entropy row. It is not a data
chunk. Schema 3.2 does not store the tx path. Mining derives that path
from the consensus database. A directory whose `SCHEMA` file is
`irys-submodule-index 3` has no content-hash family. Open refuses it and
does not create the family empty. Delete that directory and rerun
`submodule-index-convert`. A directory whose `SCHEMA` file is
`irys-submodule-index 3.1` is refused. The node does not rewrite it in
place. Run `v31-to-v32`, then `verify-rocks`, and replace `rocks/` only
after verify prints no mismatch. Schema v2 and schema 3.1 keep the raw
proof in `ChunkDataPathByOffset`. MDBX keeps that table and stores only
the proof. A schema 3.2 directory that has `ChunkDataPathByOffset` and
not `ChunkMetaByOffset` is refused. Open does not create an empty
`ChunkMetaByOffset` beside the old family. Reset that index, or
reconvert it from the schema 3.1 backup with `v31-to-v32`.

The converter is `submodule-index-convert` in the `irys-database` crate.
Build it with the `rocksdb` feature. Stop the node before you run it.
The tool takes the index lock and refuses the run when that lock is held.
The directory name does not need to contain `copy`.

`mdbx-to-rocks`, `rocks-to-v3`, `rocks-to-mdbx`, and `verify` need the
consensus database and the partition geometry. `v31-to-v32` does not.
`verify-rocks` of a schema 3.1 directory against schema 3.2, or of two
schema 3.2 directories, does not either. The consensus database is
`irys_consensus_data` under the node's `base_directory` (`.irys` by
default), not a submodule `db/`. The ledger offset is
`slot * chunks_per_partition + partition offset`. `ledger` and `slot`
are the fields in `<submodule>/packing_params.toml`.
`chunks-per-partition` and `chunk-size` come from the chain config.
Mainnet chunk size is 262144. The tool derives the tx path at that ledger
offset and requires the stored bytes to be identical. A mismatch aborts
and names the partition offset.

The converter never modifies its source. It reads a schema v1 or v2
`db/` and writes RocksDB schema 3.2 to the target. The completion marker
is written last. `rocks/SCHEMA` is the RocksDB marker.
The MDBX marker is the schema row. A crash leaves no marker. The node
refuses that directory. Run the same command again with
`--replace-incomplete`.

`rocks-to-mdbx` reads schema 3.2 only. It drops checksum-only rows and
strips the record header from proof rows. A schema v2 RocksDB index uses
`rocks-to-v3` first. A schema 3.1 index uses `v31-to-v32` first. Direct
3.1 to MDBX is refused.

The node itself migrates a schema v1 `db/` (release 4.0.x) to MDBX
schema v2 in place the first time it opens it with the MDBX engine.
Release 4.0.x cannot read the result. There is no rollback from 4.1.0 to
4.0.x. Both a schema v1 and a schema v2 `db/` convert to RocksDB, so the
conversion can run before or after the first start of 4.1.0.

## RocksDB schema 3.1 to schema 3.2

Stop the node. Do this for each submodule. The source is not modified.
The pass wraps each proof and leaves the checksum unset. It does not read
`chunks.dat`, so it writes no entropy rows. No consensus flags.

```sh
submodule-index-convert v31-to-v32 \
  <submodule>/rocks <submodule>/rocks-3.2
submodule-index-convert verify-rocks \
  <submodule>/rocks <submodule>/rocks-3.2
```

Replace `rocks/` with `rocks-3.2` only when verify-rocks prints no mismatch.
`database.submodule_index` stays `rocksdb`. Start the node. The node
deletes a leftover `chunks.crc` on open. The checksums for bytes written
after the start live in the offset row.

## RocksDB schema v2 to schema 3.2

Stop the node. Do this for each submodule. Keep `rocks/`.

```sh
submodule-index-convert rocks-to-v3 \
  --consensus-db <node>/.irys/irys_consensus_data \
  --ledger <id> --slot <slot> \
  --chunks-per-partition <n> --chunk-size <bytes> \
  <submodule>/rocks <submodule>/rocks-v3
submodule-index-convert verify-rocks \
  --consensus-db <node>/.irys/irys_consensus_data \
  --ledger <id> --slot <slot> \
  --chunks-per-partition <n> --chunk-size <bytes> \
  <submodule>/rocks <submodule>/rocks-v3
```

Replace `rocks/` with `rocks-v3` only when verify-rocks prints no mismatch.
`database.submodule_index` stays `rocksdb`. Start the node.

## Move MDBX to RocksDB schema v3

Stop the node. Do this for each submodule directory. The flags are the
same as above.

```sh
submodule-index-convert mdbx-to-rocks \
  --consensus-db <node>/.irys/irys_consensus_data \
  --ledger <id> --slot <slot> \
  --chunks-per-partition <n> --chunk-size <bytes> \
  <submodule>/db <submodule>/rocks
submodule-index-convert verify \
  --consensus-db <node>/.irys/irys_consensus_data \
  --ledger <id> --slot <slot> \
  --chunks-per-partition <n> --chunk-size <bytes> \
  <submodule>/db <submodule>/rocks
```

Set `database.submodule_index` to `rocksdb`. Start the node. Keep `db/`.

## Rollback to MDBX schema v2

This rolls a 4.1.0 node back to the MDBX engine of the same release. The
output is MDBX schema v2, which release 4.0.x cannot read. Stop the node.
`rocks-to-mdbx` rebuilds the tx paths by derivation.

```sh
submodule-index-convert rocks-to-mdbx \
  --consensus-db <node>/.irys/irys_consensus_data \
  --ledger <id> --slot <slot> \
  --chunks-per-partition <n> --chunk-size <bytes> \
  <submodule>/rocks <new-db-dir>
submodule-index-convert verify \
  --consensus-db <node>/.irys/irys_consensus_data \
  --ledger <id> --slot <slot> \
  --chunks-per-partition <n> --chunk-size <bytes> \
  <new-db-dir> <submodule>/rocks
```

Replace `db/` with `<new-db-dir>` only after verify passes. Move
`rocks/` out of the submodule directory (for example, rename it to
`rocks.retired`); the node refuses to open MDBX while `rocks/` is
present. Set `database.submodule_index` to `mdbx`. Start the node.

## Failed conversion

Run the same command again with `--replace-incomplete`. That flag deletes
a target that has no completion marker and starts the copy again. A
completed target is never replaced. `--replace-incomplete` is illegal on
`verify` and `verify-rocks`. Do not start the node on an incomplete
directory.

## Disk and time

Both directories stay on disk. Free space must cover a second copy.
Measure the live index with `du` before you start. Leave extra room for
the RocksDB WAL. Verify reads both copies.
