# Storage module indexes

<!-- MDBX-SUBMODULE-INDEX: delete the MDBX sections of this page once RocksDB v1 is the only submodule index. The consensus database stays MDBX. -->

The removal list is [mdbx-submodule-index-removal.md](mdbx-submodule-index-removal.md).

Each submodule keeps its index in `rocks/` (RocksDB v1). The node opens
only RocksDB. Releases up to 4.1.x could also keep it in `db/` (MDBX),
selected by `database.submodule_index`. That key is removed. A config that
still sets it fails to parse, so delete the key before you start this
release.

## Startup move from MDBX to RocksDB

When the node starts, it moves each submodule that still has
`db/mdbx.dat` to RocksDB before the module opens
(`crates/chain/src/submodule_index_migration.rs`):

1. Convert `db/` into `rocks.migrating/`, then verify the copy against
   `db/` and the consensus database.
2. Rename `rocks.migrating/` to `rocks/`.
3. Rename `db/` to `db.migrated/`. The node does not read it again. Keep
   it while you can still need a rollback, then delete it.

Each step leaves a state that the next start finishes. The copy reads
`db/` and does not write it, so an interrupted start deletes
`rocks.migrating/` and converts again. The one exception is an unfinished
v1 to v2 migration (see below), which the start finishes in `db/` first. A complete `rocks/` beside `db/` is the live index
(after step 2, or after a manual conversion), so only step 3 runs.

The start fails, names the module, and leaves `db/` in place when:

- `db.migrated/` already exists.
- `rocks/` beside `db/` is not a complete RocksDB v1 index: `SCHEMA` does
  not hold the v1 marker, `CURRENT` is missing, or `CONVERTING` is present.
- The consensus database does not cover a placement in `db/`. See below.
- `packing_params.toml` names no ledger, but `db/` has a placement.
- Verify finds a mismatch.

### Placement past the consensus database

The node commits the block index (`database.sync_mode = Durable` by
default) before chunk migration writes the placements, and a rollback
clears placements before it truncates the block index. So a placement can
be past the consensus frontier only when the consensus database is older
than the index: a power loss with a non-durable `database.sync_mode`, a
restored consensus database, or a module directory copied from another
node. RocksDB derives each tx path from the consensus database, so such a
placement cannot be converted, and the node cannot start to sync the
missing blocks. Recover by hand, for the submodule the error names:

```sh
submodule-index-convert mdbx-to-rocks --allow-unresolved \
  --consensus-db <node>/.irys/irys_consensus_data \
  --ledger <id> --slot <slot> \
  --chunks-per-partition <active slot width> --chunk-size <bytes> \
  <submodule>/db <submodule>/rocks
```

Do not add `--verify`: it exits 1 on every skipped placement. The copy
still compares each stored tx path it can derive with the consensus
database. The unresolved placements are left out of `rocks/` and counted as
`skipped_frontier`. Start the node. It retires `db/`, syncs the missing
blocks, and indexes those placements again when it migrates those blocks.

Open also refuses a submodule that has `db.migrated/` but no complete
`rocks/`. An empty new index would hide the rows in `db.migrated/`. To
recover, first move any broken `rocks/` out of the submodule directory.
Then restore `rocks/` from a backup, or rename `db.migrated/` back to `db/`
so the next start converts it again. `db.migrated/` is the index as it was
at the move: rows the node wrote to `rocks/` after the move are not in it.

The converter places each row at `slot * partition_chunks + offset`. The
start uses the active slot width from the epoch snapshot, which a
partition resize makes smaller than the genesis `num_chunks_in_partition`.

A schema v1 `db/` (release 4.0.8) converts directly. The converter reads
it and does not change it. A schema row that still starts with
`irys-submodule-index migrate-v1 ` is a v1 to v2 in-place migration that
an earlier release did not finish. The start finishes that migration in
`db/` (the same step that release runs on open), then converts. Copy `db/`
before the upgrade if you want to keep it as it is.

Every storage module, and every submodule inside it, converts on its own
thread, so the disks work in parallel. Memory pinned to warm the MDBX
sources shares one process memlock budget. The submodule disk needs free space for a second copy of the
index. Measure `db/` with `du` before the upgrade.

## First open

Open sets the logical size of `chunks.dat` and does not allocate blocks.
ext4 returns `EFBIG` when the file is larger than 16 TiB - 4 KiB. A
mainnet partition is 18.01 TiB, so that file does not fit. The built-in
testnet consensus (`consensus = "Testnet"`) opens that same 18.01 TiB
file. 12.37 TiB is only the Custom scaffold in `testnet_config.toml`
(`num_chunks_in_partition = 51872000`). A live testnet node does not load
that scaffold. Open refuses. `EFBIG` is not a free-space error. Format the
drive with XFS, or use another filesystem whose maximum file size is at
least 18.01 TiB. That limit applies to mainnet and to the built-in
testnet. When the file fits, the open reserves a short or holey
`chunks.dat` to the full partition (`fallocate` mode 0). A 4.0.8 file is
sparse, so the reserve runs, and on a large file that reserve can
dominate the first open. `EOPNOTSUPP` leaves the file sparse. Any other
`fallocate` error fails the open.

When the partition hash in `packing_params.toml` matches the assignment
and the ledger or the slot does not, open keeps or clears the stored
bytes by the ledger the file already names. The same hash, ledger, and
slot keeps the bytes. Capacity that gains a ledger keeps the bytes.
Leaving a ledger (another ledger, another slot, or capacity) clears
the index and rewrites the intervals to `Uninitialized`. The
`chunks.dat` bytes stay until a later pack overwrites them. A refused
open before that clear leaves the previous file in place.

Mining recall reads
`chunks.dat` with `O_DIRECT` (`storage.recall_direct_io`, default true).
A direct read of a range that still has dirty buffered pages returns the
new bytes. Linux writes that range back before the direct read (XFS and
ext4 iomap). On a filesystem that accepts `O_DIRECT` and does not write
dirty pages back first, a recall can hash bytes that are not the latest
write. An open that returns `EINVAL` or `EOPNOTSUPP` logs one
warning and keeps the buffered descriptor. Any other errno
fails module open. Durable points wait up to
`min_durable_interval_ms` (250), and `intervals.json` completion
rewrites wait up to `intervals_persist_secs` (30), once the disk lane
can run. A mining recall holds that lane until its `pread` returns.
`storage.recall_io_priority` (default true) controls that hold: when
false, a recall does not hold the disk and packing, data sync, and
serving share it with the recall preads.
A write-ahead `Interrupted` mark is still written before the `pwrite`.
`NodeConfig::testing()` sets `recall_direct_io`
false and both of those intervals to 0. A config that sets only
`num_writes_before_sync` still takes the other defaults.

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
family this binary does not know is opened with auto compaction
disabled, so this process does not rewrite that family's SSTs. The node
does not read, write, or drop it, and logs the name once. A writable
open refuses a family whose stored options set a prefix extractor.
Recovery would flush that family's WAL with default table options and
drop the extractor. A read-only open does not flush, so it still opens.

A later change that old binaries can ignore sets a `compat` or
`ro_compat` bit and keeps the RocksDB v1 marker. A change to an existing
key or value layout, width, endianness, or meaning needs a new marker
(RocksDB v2) and `submodule-index-convert`.

An existing database must already contain every RocksDB v1 family. Open
does not create a missing family. Restore the directory or reset the
storage module. A new directory, and a converter target, still create
the families.

RocksDB v1 stores the placement and the offset row in
`ChunkMetaByOffset`. The offset row is the data proof when one is stored,
and the CRC32C of the packed chunk when one was written. The data proof
is a data tree v2 chunk proof: 32-byte hashes, the siblings leaf to root,
then the size witness. It holds no chunk hash. `ChunkOffsetsByContentHash`
is retired: it stays empty (see below). A checksum with no proof is an entropy row. It is not a data
chunk.

RocksDB v1 metadata values are raw. The packing value is 41 bytes:
version `1`, the 20-byte packing address, iterations as a big-endian
`u32`, chunk size as a big-endian `u64`, and chain id as a big-endian
`u64`. The reset marker and the checksum-complete flag are the raw
byte `01`. `mdbx-to-rocks` writes the packing record when
`--packing-address`, `--packing-iterations`, and `--chain-id` are set.
The node writes it on open when they are not.

The checksum, a flags byte and a format byte are a trailer after
the proof, not a header: neighbouring proofs share their upper siblings
and the size witness, and block compression keeps more of that sharing
when each value starts with the proof (measured at 1–3 bytes per chunk less than a header; see
`encode_offset_meta`). RocksDB v1 does not store the tx path. Mining
derives that path from the consensus database. MDBX keeps
`ChunkDataPathByOffset` and stores only the proof.

## RocksDB v1 format

One RocksDB directory per submodule. The comparator is bytewise. Numeric
fields below are big-endian except the Metadata key and the CRC32C word.
Baseline compression is LZ4. Baseline block size is 64 KiB. Baseline uses
a two-level index. A bloom, when present, is a full filter at 10 bits per
key. Partitioned filters are on in the baseline when that family has a
bloom. Blob files are off in the baseline. The offset family still runs
blob GC so an older blob file can be rewritten inline.

`rocks/SCHEMA` and Metadata key 1 store the same raw bytes:
`irys-submodule-index rocksdb 1\n` (31 bytes, newline included).

A full scan of a family that has a prefix extractor sets
`total_order_seek`. A prefix lookup leaves that flag off and sets
`prefix_same_as_start`.

### ChunkMetaByOffset

Key: partition offset, `u32`, 4 bytes.

Value: the proof bytes when the proof flag is set, then the CRC32C `u32`
little-endian when the checksum flag is set, then `flags u8`, then
`format u8 = 1`. Flag `0x01` means a proof is present. Flag `0x02` means
the CRC32C word is present. Any other flag bit, or a format byte other
than 1, fails decode. A stored CRC of 0 is a real checksum. Unknown is
the checksum flag being unset. A checksum-only row is 6 bytes. A proof
with a checksum is `proof_len + 6`. A proof without a checksum is
`proof_len + 2`. An empty proof, or bytes before a checksum-only trailer,
is not a row. Both fields absent deletes the row.

No prefix extractor. Bloom is off in the baseline. It is on when the
offset-family bloom or the all-family bloom is selected.

### PlacementIntervals

Key: inclusive start, `u32`, 4 bytes.

Value: 64 bytes. `end u32` at 0, data root 32 bytes at 4, `tx_start i32`
at 36, `data_size u64` at 40, `ledger u32` at 48, `block_height u64` at
52, `tx_index u32` at 60. `u32::MAX` and `u64::MAX` read back as unset
for the ledger, the height, and the tx index.

No prefix extractor. Bloom is off unless every family has a bloom.

### DataRootPlacements

Key: data root, 32 bytes, then the interval start `u32`. 36 bytes. No
separator.

Value: 16 bytes. `end u32`, `tx_start i32`, `data_size u64`.

No prefix extractor. A read seeks the 32-byte data root. Bloom is off
unless every family has a bloom. A whole-key bloom cannot answer that
seek.

### PendingBodyMigrations

Key: clipped partition start, `u32`, 4 bytes. The MDBX name is
`PendingBodyMigrationsByOffset`. Resume deletes the old key and writes
the new key. The key is a cursor, not a stored end.

Value: 56 bytes. Data root 32 bytes at 0, `data_size u64` at 32,
unclipped `start_offset i32` at 40, `block_height u64` at 44, `attempts
u32` at 52. No unset sentinel.

No prefix extractor. Bloom is off unless every family has a bloom.

### ChunkOffsetsByContentHash

Retired. A data tree v2 chunk proof holds no chunk hash, so nothing
writes or reads this family. It is still opened with the options below,
so an existing directory keeps its layout and opens. `audit_index`
reports any row in it as a finding.

Key: content hash, 32 bytes, then the partition offset `u32`. 36 bytes.

Value: empty.

Prefix extractor `rocksdb.FixedPrefix.32`. Memtable prefix bloom ratio
0.1. `whole_key_filtering` is false. The bloom is always on, 10 bits per
prefix. A lookup of one hash sets `prefix_same_as_start` and does not set
`total_order_seek`. A scan of every hash sets `total_order_seek`.

### Metadata

Key: `u32` little-endian, 4 bytes. This is not the big-endian `u32` used
by the other families. The little-endian key matches the MDBX metadata
key.

| Key | Name | Value |
| --- | --- | --- |
| 1 | schema | Raw marker, 31 bytes. Same bytes as `rocks/SCHEMA`. |
| 2 | reset | Raw byte `01`. Readers test presence. Delete removes the key. |
| 3 | partition hash | Raw 32 bytes. Absent until the module is assigned. `clear` deletes this key. |
| 4 | packing record | 41 bytes: version `1`, packing address (20), iterations (`u32` big-endian), chunk size (`u64` big-endian), chain id (`u64` big-endian). Written on first open or by `mdbx-to-rocks`. Open checks it against the parameters valid for this module. |
| 5 | checksum scan complete | Raw byte `01`. Absent means unset. Reset deletes this key. `clear` does not. |
| 6 | feature flags | 24 bytes: `compat`, `ro_compat`, `incompat`, each a little-endian `u64`. Absent means all zeros. |

Reads are point gets of a known key. An unknown key is not scanned. No
prefix extractor. Bloom is off unless every family has a bloom.

### Feature flags

Metadata key 6, described above. RocksDB v1 sets no bits. Open refuses an
unknown `incompat` bit, refuses a writable open on an unknown `ro_compat`
bit, and ignores an unknown `compat` bit.

### default

Empty. Created so open does not attach a private cache. Same options as
Metadata, so no prefix extractor. Not a schema table.

### Ceilings

Partition offsets are `u32`. A key cannot name an offset above
`2^32 - 1`. A writer that builds a partition offset from a wider integer
uses `try_from` and returns an error.

`tx_start` is `i32`. It is a value field on the placement, the data-root
row, and the pending row. A start that does not fit in `i32` is an error.
A negative start means the tx began before this module. Big-endian order
places a negative `tx_start` after every non-negative value. No scan
orders by that field.

A placement extent is `[tx_start, tx_start + ceil(data_size / chunk_size) - 1]`.
The chunk count and a partition offset used in that comparison must fit
in `i32`. A value that does not fit is an error.

`data_size` is `u64` and is not narrowed on encode. `ledger` and
`tx_index` use `u32::MAX` as unset, so a stored id must not be that
value. `block_height` uses `u64::MAX` as unset.

An inclusive offset span is deleted and bounded as a half-open range.
The exclusive end is `end + 1`. When `end` is `u32::MAX`, the end key is
the 4-byte max key plus a trailing `0`, which sorts after every 4-byte
offset.

The converter is `submodule-index-convert` in the `irys-database` crate.
Stop the node before you run it.
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
`--config` is the `config.toml` of the node that wrote this index. The tool
takes the chunk size and the data tree geometry from its consensus config.
The geometry derives each tx's data commitment, and a stored placement with
another commitment is a mismatch. The default slot width is that config's
`num_chunks_in_partition`. After `[hardforks.fission]` is active, pass
`--chunks-per-partition` as the resized `partition_chunks`: the tool has no
epoch snapshot, so it cannot see the active width itself. Every node of one
chain has the same consensus config, so any node's config of that chain
works. A config for another chain or preset, or another custom consensus
file, gives false mismatches. When `consensus` in that file is a `Path`, a
relative path resolves from the current directory: run the tool from the
node's working directory or use an absolute path. Mainnet chunk size is
262144. The tool derives the tx path at that ledger offset and requires the
stored bytes to be identical. A mismatch aborts and names the partition
offset.

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

A directory whose `rocks/SCHEMA` is the RocksDB v1 marker and that has no
`rocks/CURRENT` is not a new database. `CURRENT` names the manifest. Open
refuses that directory and does not create an empty index. Restore
`CURRENT`, then start the node. Any other marker is an unrecognised layout.
Open does not rewrite it.

`rocks-to-mdbx` reads RocksDB v1 only. It drops checksum-only rows and
copies the proof bytes from the offset record. It does not copy the
trailer.

## Move MDBX to RocksDB v1 by hand

The node does this when it starts. To do it before the upgrade, stop the
node and run, for each submodule directory. `--config` supplies the chunk
size, the data tree geometry, and the default slot width. Add
`--chunks-per-partition` when a partition resize has changed the active
width. That width is the resized `partition_chunks`, not the genesis
`num_chunks_in_partition`.

```sh
submodule-index-convert mdbx-to-rocks --verify \
  --consensus-db <node>/.irys/irys_consensus_data \
  --ledger <id> --slot <slot> \
  --config <node>/config.toml \
  <submodule>/db <submodule>/rocks
```

The next start sees the complete `rocks/` and renames `db/` to
`db.migrated/`.

## Rollback to 4.1.x

A 4.1.x node reads `rocks/` when its config sets
`database.submodule_index = "rocksdb"`. Add that key and start the 4.1.x
node. Leave `db.migrated/` in place: 4.1.x does not read it.

To roll back to the MDBX engine of 4.1.x instead, build a new MDBX index
from `rocks/` as below.

## Rollback to MDBX schema v2

This rolls a node back to the MDBX engine of a 4.1.x release.
Release 4.0.x cannot read it. Stop the node. `rocks-to-mdbx` rebuilds the
tx paths by derivation. `db.migrated/` does not have the writes made
after the move, so do not use it as the rollback index.

```sh
submodule-index-convert rocks-to-mdbx \
  --consensus-db <node>/.irys/irys_consensus_data \
  --ledger <id> --slot <slot> \
  --config <node>/config.toml \
  <submodule>/rocks <new-db-dir>
submodule-index-convert verify --rollback \
  --consensus-db <node>/.irys/irys_consensus_data \
  --ledger <id> --slot <slot> \
  --config <node>/config.toml \
  <new-db-dir> <submodule>/rocks
```

`--rollback` is required here: MDBX has no checksum column, so the
checksums the node wrote on RocksDB are reported as `dropped_checksums`,
not as mismatches. After verify passes, put `<new-db-dir>` at `db/`. Move
`rocks/` and `db.migrated/` out of the submodule directory (for example,
rename `rocks/` to `rocks.retired`). The 4.1.x open refuses MDBX when
`rocks/SCHEMA` or `rocks/CURRENT` is a file. Set
`database.submodule_index` to `mdbx` or delete the key, then start the
4.1.x node.

## Corrupt intervals.json

Stop the node. Each submodule directory has an `intervals.json`. It
records which offsets are `Data`, `Entropy`, `Uninitialized`, or
`Interrupted`. Open fails when that file cannot be parsed, and the error
names the path.

Move that `intervals.json` aside. Clear this submodule directory
(index, intervals.json, chunks.dat) and start the node. The module packs again.
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
