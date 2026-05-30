# Contributing to mnestic

mnestic is a maintained fork of [CozoDB](https://github.com/cozodb/cozo) under
MPL-2.0. See [`FORK.md`](FORK.md) for provenance and licensing, and
[`CHANGELOG-FORK.md`](CHANGELOG-FORK.md) for divergences from upstream.

> This fork does **not** use the upstream Cozo CLA. Contributions are accepted
> under the project's MPL-2.0 license (see below).

## Building & testing

The workspace's active members are `cozo-core` (published as the `mnestic`
crate), `cozorocks` (RocksDB bridge), `cozo-bin`, and `cozo-core-examples`.

```bash
# Fast path — default features are SQLite-only (no C++ build):
cargo build -p mnestic
cargo test  -p mnestic            # engine unit tests + integration tests

# RocksDB backend (compiles the C++ bridge; needs submodules):
git submodule update --init --recursive
cargo build -p mnestic --features storage-rocksdb
```

The published package is `mnestic`, but the importable crate name is `cozo`, so
existing CozoDB code (`use cozo::...`) works unchanged.

### A note on tests

The in-memory backend uses a different join operator than the persistent
backends, so tests that exercise stored-relation or planner behavior should use
the **SQLite** backend with a temporary directory (see
`cozo-core/tests/matjoin_regression.rs`). Add a failing test that reproduces the
issue before fixing it.

## Pull requests

- Branch from `main`; keep changes focused.
- Don't break the inherited upstream test suite — `cargo test -p mnestic` must pass.
- Preserve the per-file `Copyright … The Cozo Project Authors` MPL headers on any
  file you modify.
- Note any user-visible divergence from upstream in `CHANGELOG-FORK.md`.
- CI (`.github/workflows/build.yml`) runs build + tests on every push/PR.

## License

By contributing you agree your contributions are licensed under MPL-2.0.
