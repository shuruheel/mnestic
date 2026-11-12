#!/usr/bin/env bash
# Clean-environment check of the Python quickstart (docs/guides/quickstart.md).
#
# Creates a fresh virtualenv outside the repository, installs mnestic the way a new
# user would, and runs cozo-lib-python/examples/quickstart.py from an empty directory
# with `python -I` (no repository files on the import path). Exits non-zero if any
# quickstart assertion fails.
#
#   scripts/check-quickstart.sh                 # the version in cozo-lib-python/pyproject.toml, from PyPI
#   scripts/check-quickstart.sh 0.19.0          # a specific PyPI version
#   scripts/check-quickstart.sh path/to/x.whl   # a candidate wheel, before publishing
#
# Run it for the release candidate wheel and again after publishing.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
target="${1:-}"
if [[ -z "$target" ]]; then
  target="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/cozo-lib-python/pyproject.toml" | head -1)"
fi
if [[ -f "$target" ]]; then
  spec="$(cd "$(dirname "$target")" && pwd)/$(basename "$target")"
else
  spec="mnestic==$target"
fi

python="${PYTHON:-python3}"
work="$(mktemp -d "${TMPDIR:-/tmp}/mnestic-quickstart.XXXXXX")"
trap 'rm -rf "$work"' EXIT

echo "== installing $spec into a fresh venv ($("$python" --version))"
"$python" -m venv "$work/venv"
"$work/venv/bin/python" -m pip install --quiet --upgrade pip
"$work/venv/bin/python" -m pip install --quiet --no-cache-dir "$spec"
"$work/venv/bin/python" -m pip show mnestic | sed -n 's/^Version: /installed mnestic /p'

echo "== running the quickstart from an empty directory"
mkdir "$work/run"
cp "$root/cozo-lib-python/examples/quickstart.py" "$work/run/"
(cd "$work/run" && "$work/venv/bin/python" -I quickstart.py "$work/run/store")
echo "== quickstart passed"
