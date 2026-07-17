#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
expected_new="$(git -C "$repo_root" rev-parse correctness-0.13.0)"
actual_new="$(git -C "$repo_root" rev-parse HEAD)"

if [[ "$actual_new" != "$expected_new" ]]; then
  echo "error: run from correctness-0.13.0 (expected $expected_new, got $actual_new)" >&2
  exit 1
fi

run_root="${1:-$(mktemp -d "${TMPDIR:-/tmp}/mnestic-upgrade-0130.XXXXXX")}"
mkdir -p "$run_root"
run_root="$(cd "$run_root" && pwd -P)"
fixtures="$run_root/fixtures"
old_worktree="$run_root/mnestic-0122"
old_target="$run_root/target-0122"

mkdir -p "$fixtures"

cleanup_worktree() {
  git -C "$repo_root" worktree remove --force "$old_worktree" 2>/dev/null || true
}
trap cleanup_worktree EXIT

git -C "$repo_root" worktree add --detach "$old_worktree" v0.12.2
cp "$repo_root/cozo-core/tests/upgrade_0130.rs" \
  "$old_worktree/cozo-core/tests/upgrade_0130.rs"

echo "==> Phase 1/3: seed affected stores with v0.12.2"
MNESTIC_UPGRADE_PHASE=seed \
MNESTIC_UPGRADE_ROOT="$fixtures" \
CARGO_TARGET_DIR="$old_target" \
  cargo test --manifest-path "$old_worktree/Cargo.toml" -p mnestic \
    --test upgrade_0130 upgrade_0130_rehearsal -- --ignored --nocapture

echo "==> Phase 2/3: execute upgrade instructions with correctness-0.13.0"
MNESTIC_UPGRADE_PHASE=verify \
MNESTIC_UPGRADE_ROOT="$fixtures" \
  cargo test --manifest-path "$repo_root/Cargo.toml" -p mnestic \
    --test upgrade_0130 upgrade_0130_rehearsal -- --ignored --nocapture

echo "==> Phase 3/3: reopen updated stores with v0.12.2"
MNESTIC_UPGRADE_PHASE=backward \
MNESTIC_UPGRADE_ROOT="$fixtures" \
CARGO_TARGET_DIR="$old_target" \
  cargo test --manifest-path "$old_worktree/Cargo.toml" -p mnestic \
    --test upgrade_0130 upgrade_0130_rehearsal -- --ignored --nocapture

CARGO_TARGET_DIR="$old_target" \
  cargo clean --manifest-path "$old_worktree/Cargo.toml"

echo "==> PASS: composite 0.12.2 -> 0.13.0 upgrade rehearsal"
echo "    fixtures: $fixtures"
