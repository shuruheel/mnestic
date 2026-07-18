"""Per-OS data/model directory resolution. No platformdirs dependency."""

from __future__ import annotations

import os
import sys
from pathlib import Path


def data_dir() -> Path:
    if sys.platform == "darwin":
        base = Path.home() / "Library" / "Application Support"
    elif os.name == "nt":
        base = Path(os.environ.get("LOCALAPPDATA", str(Path.home() / "AppData" / "Local")))
    else:
        base = Path(os.environ.get("XDG_DATA_HOME", str(Path.home() / ".local" / "share")))
    return base / "mnestic-mcp"


def default_db_path() -> str:
    env = os.environ.get("MNESTIC_MCP_DB")
    if env:
        return env
    d = data_dir()
    d.mkdir(parents=True, exist_ok=True)
    return str(d / "memory.db")


def model_cache_dir() -> str:
    env = os.environ.get("MNESTIC_MCP_CACHE")
    if env:
        return env
    d = data_dir() / "models"
    d.mkdir(parents=True, exist_ok=True)
    return str(d)
