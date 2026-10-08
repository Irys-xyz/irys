# Storage module indexes

Each submodule keeps its index in `db/` (MDBX) or `rocks/` (RocksDB).
`database.submodule_index` selects the engine. The value is `mdbx` or
`rocksdb`. The node opens only that directory. It refuses a layout that
does not match the config.

The converter is `submodule-index-convert` in the `irys-database` crate.
Build it with the `rocksdb` feature. Stop the node before you run it.
The tool takes the index lock and refuses the run when that lock is held.
The directory name does not need to contain `copy`.

The source is not modified, except a schema v1 `db/`. That directory is
migrated to v2 in place, and then copied. The completion marker is written
last. `rocks/SCHEMA` is the RocksDB marker. The MDBX marker is the schema
row. A crash leaves no marker. The node refuses that directory.

## Move MDBX to RocksDB

Stop the node. Do this for each submodule directory.

```sh
submodule-index-convert mdbx-to-rocks <submodule>/db <submodule>/rocks
submodule-index-convert verify <submodule>/db <submodule>/rocks
```

Set `database.submodule_index` to `rocksdb`. Start the node. Keep `db/`.

## Rollback

If the node wrote no new rows into `rocks/` after the switch, set
`database.submodule_index` back to `mdbx` and start the node. `db/` is
still the index from the conversion.

If the node wrote new rows into `rocks/`, copy those rows back before the
switch. A config change alone leaves them out of `db/`.

```sh
submodule-index-convert rocks-to-mdbx <submodule>/rocks <new-db-dir>
submodule-index-convert verify <new-db-dir> <submodule>/rocks
```

Replace `db/` with `<new-db-dir>` only after verify passes. Set
`database.submodule_index` to `mdbx`. Start the node.

## Failed conversion

Run the same command again with `--replace-incomplete`. That flag deletes
a target that has no completion marker and starts the copy again. A
completed target is never replaced. Do not start the node on an incomplete
directory.

## Disk and time

Both directories stay on disk. Free space must cover a second copy.
Measure the live index with `du` before you start. Leave extra room for
the RocksDB WAL.

One partition of 18 TiB holds 75,497,472 chunks of 256 KiB. A devnet index
of 306,597 path-hash rows grew from 815 MB to 1.4 GB when it moved to
schema v2. Scaled to one data path on every chunk, the index is on the
order of 350 GiB. RocksDB may be smaller because it compresses values.
On an SSD the copy is on the order of tens of minutes. A slow disk can
take about an hour. Verify reads both copies and takes a similar time.
