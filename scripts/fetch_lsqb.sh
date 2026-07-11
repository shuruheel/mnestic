#!/usr/bin/env bash
#
# Fetch the LSQB sf0.1 dataset for the T1 execution tier
# (cozo-core/tests/lsqb.rs). See docs/plans/planner-regression-suite.md.
#
# LSQB is LDBC's Labelled Subgraph Query Benchmark (github.com/ldbc/lsqb,
# Apache-2.0). We use the *projected-fk* layout: one relation per edge type, no
# nullable columns. The merged-fk twin encodes absent FKs as NULL, and Cozo has
# no NULL — loading it would either fail or silently coerce.
#
# 6.1 MB. No generator, no Docker, no Spark, no auth.
#
#   ./scripts/fetch_lsqb.sh                 # -> .lsqb/social-network-sf0.1-projected-fk
#   LSQB_SF01_DIR=/some/where cargo test --release -p mnestic --test lsqb -- --ignored
#
set -euo pipefail

URL="https://datasets.ldbcouncil.org/lsqb/social-network-sf0.1-projected-fk.tar.zst"
SHA256="20b08cfbc0b765bb066135a4c8d99367fb4f0d5c500a63b725e258dcb91b7005"
DEST="${LSQB_CACHE_DIR:-.lsqb}"
ARCHIVE="$DEST/social-network-sf0.1-projected-fk.tar.zst"
EXTRACTED="$DEST/social-network-sf0.1-projected-fk"

mkdir -p "$DEST"

if [ -d "$EXTRACTED" ] && [ -f "$EXTRACTED/Person_knows_Person.csv" ]; then
  echo "LSQB sf0.1 already present: $EXTRACTED"
else
  if [ ! -f "$ARCHIVE" ]; then
    echo "==> Fetching $URL"
    curl -fsSL --retry 3 -o "$ARCHIVE" "$URL"
  fi

  # Pin the dataset. A silent upstream re-cut would move the expected counts,
  # and a count oracle that drifts with its own input is not an oracle.
  echo "==> Verifying sha256"
  if command -v sha256sum >/dev/null 2>&1; then
    echo "$SHA256  $ARCHIVE" | sha256sum -c -
  else
    echo "$SHA256  $ARCHIVE" | shasum -a 256 -c -
  fi

  echo "==> Extracting"
  tar --use-compress-program=unzstd -xf "$ARCHIVE" -C "$DEST"
fi

echo "LSQB_SF01_DIR=$(cd "$EXTRACTED" && pwd)"
