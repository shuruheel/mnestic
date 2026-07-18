"""`mnestic-mcp` entry point: resolve paths, open the db, start the embedder
warmup in the background, serve MCP over stdio."""

from __future__ import annotations

import argparse
import logging
import os
import sys

from mnestic_mcp import __version__
from mnestic_mcp.embeddings import (
    DEFAULT_MODEL,
    FastEmbedEmbedder,
    configure_stderr_logging,
)
from mnestic_mcp.memory import MemoryStore
from mnestic_mcp.paths import default_db_path, model_cache_dir
from mnestic_mcp.server import build_server

logger = logging.getLogger("mnestic-mcp")


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        prog="mnestic-mcp",
        description="Local memory MCP server backed by mnestic (stdio transport).",
    )
    parser.add_argument("--db", default=None, help="database path (default: per-OS data dir, or MNESTIC_MCP_DB)")
    parser.add_argument(
        "--engine",
        default=os.environ.get("MNESTIC_MCP_ENGINE", "sqlite"),
        choices=["sqlite", "rocksdb", "mem"],
        help="storage engine (default sqlite; rocksdb is single-instance only)",
    )
    parser.add_argument(
        "--model",
        default=os.environ.get("MNESTIC_MCP_MODEL", DEFAULT_MODEL),
        help=f"fastembed model name (default {DEFAULT_MODEL})",
    )
    parser.add_argument(
        "--download-model",
        action="store_true",
        help="download/load the embedding model, then exit (for provisioning)",
    )
    parser.add_argument("--version", action="version", version=f"mnestic-mcp {__version__}")
    args = parser.parse_args(argv)

    configure_stderr_logging()

    embedder = FastEmbedEmbedder(model_name=args.model, cache_dir=model_cache_dir())
    if args.download_model:
        embedder.wait_ready(timeout=3600.0)
        logger.info("model %s ready in cache", args.model)
        return 0

    db_path = args.db or (default_db_path() if args.engine != "mem" else "")
    from mnestic import CozoDbPy

    try:
        db = CozoDbPy(args.engine, db_path, "{}")
    except Exception as e:
        if args.engine == "rocksdb" and "lock" in str(e).lower():
            logger.error(
                "another mnestic-mcp instance already has %s open (rocksdb is "
                "single-instance). Close it, or use --engine sqlite.",
                db_path,
            )
            return 1
        raise

    store = MemoryStore(db, embedder, db_path=db_path, engine=args.engine)
    embedder.start_warmup()  # download races the first minutes of conversation
    logger.info("serving memory at %s (%s), model %s", db_path or "<mem>", args.engine, args.model)
    build_server(store).run()
    return 0


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
