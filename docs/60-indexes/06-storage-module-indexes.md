# Storage module indexes

Each submodule keeps its index in `db/` (MDBX) or `rocks/` (RocksDB).
`database.submodule_index` selects the engine. The value is `mdbx` or
`rocksdb`. The node opens only that directory. It refuses a layout that
does not match the config. The node refuses to open MDBX when `rocks/`
also exists, because after a conversion `rocks/` is the live index and
`db/` stays beside it unread. A rollback to MDBX moves `rocks/` out of
the module directory (for example, rename it to `rocks.retired`) before
the node starts.

MDBX schema v1 is the on-disk format of release 4.0.8. The node migrates
that `db/` to MDBX schema v2 in place the first time this release opens
it. Release 4.0.x cannot read the result. There is no rollback from
4.1.0 to 4.0.x. Both a schema v1 and a schema v2 `db/` convert to
RocksDB, so the conversion can run before or after the first start.

A fresh RocksDB index is RocksDB v1. The marker, in `rocks/SCHEMA` and
in the metadata schema row, is `irys-submodule-index rocksdb 1`. Any
other marker is refused. This build does not rewrite it. Move the
directory aside and convert from MDBX, or reset the storage module.

Feature flags are a metadata row of 24 bytes: three little-endian u64
bitsets, `compat`, then `ro_compat`, then `incompat`. A missing row
means all zeros. This release sets no bits. An unknown `incompat` bit
refuses every open. The error names the bits and says this binary is
too old. An unknown `ro_compat` bit refuses a writable open and still
allows a read-only open. An unknown `compat` bit is ignored.

Open reads the on-disk column-family list and opens every family. A
family this binary does not know gets RocksDB's default options. The
node does not read, write, or drop it, and logs the name once. A family
that needs another comparator or a prefix extractor is not ignorable.

A later change that old binaries can ignore sets a `compat` or
`ro_compat` bit and keeps the RocksDB v1 marker. A change to an existing
key or value layout, width, endianness, or meaning needs a new marker
(RocksDB v2) and `submodule-index-convert`.

An existing database must already contain every RocksDB v1 family. Open
does not create a missing family. Restore the directory or reset the
storage module. A new directory, and a converter target, still create
the families.

RocksDB v1 stores the placement, the offset row in `ChunkMetaByOffset`,
and `ChunkOffsetsByContentHash` (the 32-byte leaf hash, then the
partition offset, with an empty value). The offset row is the data proof
when one is stored, and the CRC32C of the packed chunk when one was
written. A checksum with no proof is an entropy row. It is not a data
chunk.

RocksDB v1 metadata values are raw. The packing value is 41 bytes:
version `1`, the 20-byte packing address, iterations as a big-endian
`u32`, chunk size as a big-endian `u64`, and chain id as a big-endian
`u64`. The reset marker and the checksum-complete flag are the raw
byte `01`. `mdbx-to-rocks` writes the packing record when
`--packing-address`, `--packing-iterations`, and `--chain-id` are set.
The node writes it on open when they are not.

The checksum, a flags byte and a format byte are a trailer after
the proof, not a header: neighbouring proofs share their upper branches,
and block compression keeps more of that sharing when each value starts
with the proof (measured at 1–3 bytes per chunk less than a header; see
`encode_offset_meta`). RocksDB v1 does not store the tx path. Mining
derives that path from the consensus database. MDBX keeps
`ChunkDataPathByOffset` and stores only the proof.

The converter is `submodule-index-convert` in the `irys-database` crate.
Build it with the `rocksdb` feature. Stop the node before you run it.
The tool takes the index lock and refuses the run when that lock is held.
The directory name does not need to contain `copy`.

`mdbx-to-rocks`, `rocks-to-mdbx`, and `verify` need the consensus
database and the partition geometry. `verify-rocks` byte-compares two RocksDB
v1 directories and does not. The consensus database is
`irys_consensus_data` under the node's `base_directory` (`.irys` by
default), not a submodule `db/`. The ledger offset is
`slot * chunks_per_partition + partition offset`. `ledger` and `slot`
are the fields in `<submodule>/packing_params.toml`.
That file also records `entropy_packing_iterations` and `chunk_size`.
A file that omits them is filled from the current consensus when the
node opens the module, then written back.
`chunks-per-partition` and `chunk-size` come from the chain config.
Mainnet chunk size is 262144. The tool derives the tx path at that ledger
offset and requires the stored bytes to be identical. A mismatch aborts
and names the partition offset.

The converter never modifies its source. It reads a schema v1 or v2
`db/` and writes RocksDB v1 to the target. The completion marker is
written last. `rocks/SCHEMA` is the RocksDB marker. The MDBX marker is
the schema row. A crash leaves no marker and leaves `CONVERTING`. The
node refuses that directory. Run the same command again with
`--replace-incomplete`.

A first open that crashes after RocksDB creates `CURRENT` and before it
writes `SCHEMA` has no `CONVERTING` file. Delete that `rocks/` directory
and start the node again. `--replace-incomplete` only deletes a
directory that contains `CONVERTING`.

`rocks-to-mdbx` reads RocksDB v1 only. It drops checksum-only rows and
copies the proof bytes from the offset record. It does not copy the
trailer.

## Move MDBX to RocksDB v1

Stop the node. Do this for each submodule directory.

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

## Corrupt intervals.json

Stop the node. Each submodule directory has an `intervals.json`. It
records which offsets are `Data`, `Entropy`, `Uninitialized`, or
`Interrupted`. Open fails when that file cannot be parsed, and the error
names the path.

Move that `intervals.json` aside. Clear the contents of that storage
module's submodule directory and start the node. The module packs again.
Do not write `Data` back by hand. A span the node cannot trust is
`Uninitialized`, and open drops presence rows that are not under `Data`.

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
