# Storage module indexes

Each submodule keeps its index in `db/` (MDBX) or `rocks/` (RocksDB).
`database.submodule_index` selects the engine. The value is `mdbx` or
`rocksdb`. The node opens only that directory. It refuses a layout that
does not match the config. The node refuses to open MDBX when
`rocks/SCHEMA` or `rocks/CURRENT` exists, because after a conversion
`rocks/` is the live index and `db/` stays beside it unread. A `rocks/`
directory that contains only `CONVERTING` does not count. A rollback to
MDBX moves `rocks/` out of the module directory (for example, rename it
to `rocks.retired`) before the node starts.

MDBX schema v1 is the on-disk format of release 4.0.8. The node migrates
that `db/` to MDBX schema v2 in place the first time this release opens
it. Release 4.0.x cannot read the result. There is no rollback from
4.1.0 to 4.0.x. Open blocks on that migration: it copies every data path
into a per-offset table in batches, then drops the old hash-keyed table,
so the module is not served until it finishes and free space must hold
both copies until the drop. A v1 data-path hash with no stored blob
refuses open, and each later start retries it, until the file is
repaired or the module is reset. Both a finished schema v1 `db/` and a
schema v2 `db/` convert to RocksDB, so the conversion can run before or
after the first start. A schema row that still starts with
`irys-submodule-index migrate-v1 ` is neither. The converter refuses it
and does not modify the directory. Start the node so the in-place
migration finishes, then convert. There is no separate v1 copy.

An omitted `database.submodule_index` stays `mdbx`. That first open is
also when these storage defaults take effect. Mining recall reads
`chunks.dat` with `O_DIRECT` (`storage.recall_direct_io`, default true).
An open that returns `EINVAL` or `EOPNOTSUPP` (tmpfs returns `EINVAL`)
logs one warning and keeps the buffered descriptor. Any other errno
fails module open. Durable points wait up to
`min_durable_interval_ms` (250). `intervals.json` completion rewrites
wait up to `intervals_persist_secs` (30). A write-ahead `Interrupted`
mark is still immediate. `NodeConfig::testing()` sets the recall flag
false and both of those intervals to 0. A config that sets only
`num_writes_before_sync` still takes the other defaults. The RocksDB
cache and write-buffer keys are ignored while the engine stays MDBX.

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
does not read, write, or drop it, and logs the name once. A family that
needs another comparator or a prefix extractor is not ignorable.

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

Key: leaf content hash, 32 bytes, then the partition offset `u32`. 36
bytes. The hash is the leaf in the last 64 bytes of the proof. A shorter
proof has no row. A checksum-only row has no row. The checksum is not
part of the key.

Value: empty. A non-empty value is a finding.

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

Point gets only. An unknown discriminant is not decoded. No prefix
extractor. Bloom is off unless every family has a bloom.

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

A directory that has `rocks/SCHEMA` and no `rocks/CURRENT` is not a new
database. `CURRENT` names the manifest. Open refuses that directory and
does not create an empty index. Restore `CURRENT`, then start the node.

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
submodule-index-convert verify --rollback \
  --consensus-db <node>/.irys/irys_consensus_data \
  --ledger <id> --slot <slot> \
  --chunks-per-partition <n> --chunk-size <bytes> \
  <new-db-dir> <submodule>/rocks
```

`--rollback` is required here: MDBX has no checksum column, so the
checksums the node wrote on RocksDB are reported as `dropped_checksums`,
not as mismatches. Replace `db/` with `<new-db-dir>` only after verify passes. Move
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
