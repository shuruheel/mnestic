#!/usr/bin/env python3
"""
check_versions.py — intra-repo version-consistency check for the mnestic engine.

Self-contained: it takes cozo-core/Cargo.toml `[package].version` as the single
in-repo reference and asserts every OTHER place that must echo that number (or a
number derived from it) agrees. This is the mnestic half of the ecosystem-wide
`release-tools/check_ecosystem.py`, re-implemented WITHOUT the sibling
`versions.toml` because per-repo CI only checks out THIS repo — it cannot see the
ecosystem manifest or sibling repos. So the repo's own primary manifest is the
reference. It catches the "bumped cozo-core but forgot cozo-lib-python / a pin"
class of drift on every PR.

The cross-repo seams (mnestic == mindgraph-rs's engine pin, git tags, etc.) are
NOT checkable here and live in the ecosystem checker instead.

NOTE: cozo-bin's own `[package].version` (0.7.6, an upstream relic) and the
repo-root `VERSION` file are NOT the engine version — deliberately unchecked.

Requires Python 3.11+ (tomllib). No third-party deps. Exit 0 = all pass, 1 = any fail.
"""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

# scripts/ lives at <repo>/scripts/, so the repo root is the parent of parent.
ROOT = Path(__file__).resolve().parent.parent

# ── ANSI (suppressed when not a tty) ──────────────────────────────────────────
_tty = sys.stdout.isatty()
def _c(code: str, s: str) -> str: return f"\033[{code}m{s}\033[0m" if _tty else s
def dim(s): return _c("2", s)
def bold(s): return _c("1", s)
def green(s): return _c("32", s)
def red(s): return _c("31", s)

_fails: list[str] = []
_oks = 0

def check(ok: bool, label: str, detail: str = "") -> None:
    global _oks
    if ok:
        _oks += 1
        print(f"  {green('✓')} {label}" + (dim(f"  {detail}") if detail else ""))
    else:
        _fails.append(label)
        print(f"  {red('✗')} {label}" + (f"  {red(detail)}" if detail else ""))


# ── parsing helpers (mirrors release-tools/check_ecosystem.py) ─────────────────
def load_toml(p: Path) -> dict:
    with open(p, "rb") as f:
        return tomllib.load(f)

def toml_version(p: Path) -> str | None:
    """`[package].version` (Cargo.toml) or `[project].version` (pyproject.toml)."""
    if not p.exists():
        return None
    d = load_toml(p)
    return d.get("package", {}).get("version") or d.get("project", {}).get("version")

def cargo_dep_req(p: Path, package: str) -> str | None:
    """Version requirement of the cargo dep whose `package = "<package>"`.

    Matches the rename idiom `cozo = { package = "mnestic", version = "X", ... }`
    and also a same-named dep `cozorocks = { version = "X" }` (no package key)."""
    if not p.exists():
        return None
    for line in p.read_text().splitlines():
        s = line.strip()
        if s.startswith("#"):
            continue
        if re.search(rf'package\s*=\s*"{re.escape(package)}"', s):
            m = re.search(r'version\s*=\s*"([^"]+)"', s)
            if m:
                return m.group(1)
        m = re.match(rf'{re.escape(package)}\s*=\s*\{{[^}}]*version\s*=\s*"([^"]+)"', s)
        if m:
            return m.group(1)
    return None

def norm(req: str | None) -> str | None:
    """Drop a leading caret/tilde/= so an equality contract compares numbers."""
    return req.lstrip("^~= ").strip() if req else req


def main() -> int:
    eng = toml_version(ROOT / "cozo-core/Cargo.toml")
    if not eng:
        print(red("could not read cozo-core/Cargo.toml [package].version"))
        return 1

    print(bold(f"\nmnestic intra-repo version check · reference {eng}  ({ROOT})\n"))

    # cozo-lib-python: both the Python project version and the Rust crate version.
    check(toml_version(ROOT / "cozo-lib-python/pyproject.toml") == eng,
          "cozo-lib-python/pyproject.toml version",
          f"want {eng}, got {toml_version(ROOT/'cozo-lib-python/pyproject.toml')}")
    check(toml_version(ROOT / "cozo-lib-python/Cargo.toml") == eng,
          "cozo-lib-python/Cargo.toml version",
          f"want {eng}, got {toml_version(ROOT/'cozo-lib-python/Cargo.toml')}")

    # workspace dependents pin the engine via the `package = "mnestic"` rename idiom.
    for sub in ("cozo-bin", "cozo-core-examples"):
        req = norm(cargo_dep_req(ROOT / sub / "Cargo.toml", "mnestic"))
        check(req == eng, f"{sub} cozo-dep pin", f"want {eng}, got {req}")

    # mnestic-rocks bridge: its own [package].version must equal the pin cozo-core
    # holds for it (the C++ bridge has its OWN cadence, so it's the reference here).
    rocks = norm(cargo_dep_req(ROOT / "cozo-core/Cargo.toml", "mnestic-rocks"))
    rocks_pkg = toml_version(ROOT / "cozorocks/Cargo.toml")
    check(rocks is not None and rocks_pkg == rocks,
          "cozorocks/Cargo.toml version == cozo-core mnestic-rocks pin",
          f"pin {rocks}, got {rocks_pkg}")

    # immutable crates.io install snippet — must name the current engine version.
    rm = ROOT / "cozo-core/README-mnestic.md"
    check(rm.exists() and f'mnestic = "{eng}"' in rm.read_text(),
          "README-mnestic.md install snippet (crates.io, immutable)",
          f'expects  mnestic = "{eng}"')

    # changelog entry present for this version.
    cl = ROOT / "CHANGELOG-FORK.md"
    check(cl.exists() and re.search(rf"(?m)^##\s+{re.escape(eng)}\b", cl.read_text()) is not None,
          f"CHANGELOG-FORK.md has a ## {eng} heading")

    print(bold("\n── summary ──"))
    print(f"  {green(str(_oks) + ' ok')}   {red(str(len(_fails)) + ' fail')}")
    if _fails:
        print(red(f"\nFAIL — intra-repo versions are out of sync with cozo-core {eng}. "
                  "Fix the ✗ items above."))
        return 1
    print(green(f"\nOK — every in-repo location agrees with cozo-core {eng}."))
    return 0


if __name__ == "__main__":
    sys.exit(main())
