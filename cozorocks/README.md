# Cozorocks

Bindings to RocksDB's C++ API.

## Build cache

Compiling the vendored RocksDB takes several minutes, and Cargo repeats it for
every target directory (each worktree, CI job, or `cargo clean`). Set
`MNESTIC_ROCKSDB_CACHE_DIR` to reuse the compiled archive instead:

```toml
# ~/.cargo/config.toml
[env]
MNESTIC_ROCKSDB_CACHE_DIR = "/home/me/.cache/mnestic-rocksdb"
```

The first build stores `librocksdb.a` under `<dir>/<fingerprint>/`; later builds
hard-link it into `OUT_DIR` (or copy it across filesystems). The fingerprint
covers the RocksDB sources and headers, `build.rs`, the C++ compiler's version
and arguments (target, profile, `CXXFLAGS`), enabled features, and the zstd/lz4
headers, so any change builds and stores a new entry. Entries are never pruned:
delete the directory to reclaim space.

To link a RocksDB you built yourself instead, set `ROCKSDB_LIB_DIR` (and
`ROCKSDB_STATIC` for a static archive).
