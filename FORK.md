# mnestic — a maintained fork of CozoDB

**mnestic** is a hard fork of [CozoDB](https://github.com/cozodb/cozo), a
transactional relational-graph-vector database that uses Datalog for queries.
It is maintained independently as a substrate for agentic memory systems.

## Provenance

- **Forked from:** [`cozodb/cozo`](https://github.com/cozodb/cozo)
- **Fork point:** commit `481af058abac9444ea8c9c52c78f096ed4b5bfc4` (2024-12-04),
  the last commit on upstream `main` before it went dormant. Tagged `fork-base`
  in this repository.
- The complete upstream git history is preserved in this repository. The
  original `origin` remote is retained under the name `upstream`.

## Relationship to CozoDB

mnestic is an **independent continuation** of CozoDB. It is **not** the official
CozoDB project, is **not** endorsed by or affiliated with the original authors,
and does **not** claim the CozoDB name, logo, or package identities.

CozoDB's original maintainer, **Ziyang Hu**, and the Cozo Project Authors created
the work this fork is built on. All credit for the original design and
implementation belongs to them. CozoDB is licensed under the
Mozilla Public License 2.0 (see `LICENSE.txt`); mnestic remains under the same
license.

Per MPL-2.0 §2.3, no trademark rights are granted by the license. We therefore
operate under our own name (`mnestic`) and our own package identities on
crates.io / npm / PyPI, rather than reusing the `cozo`/`cozo-*` names.

## What changed in the fork

See `CHANGELOG-FORK.md` for the running list of divergences from upstream
`481af05`. At a high level the fork's roadmap is to make Cozo's engine a
first-class agentic-memory substrate: performance fixes to the write/ingest and
HNSW paths, correctness fixes to the query planner, and operational tooling for
long-running graph memory.

## License

Mozilla Public License 2.0. Original copyright © 2022 The Cozo Project Authors.
Fork modifications © 2026 Shan Rizvi. Per-file copyright notices from
upstream are preserved; modified files retain the original notice as required by
MPL-2.0 §3.4.
