"""Embedding provider. Production uses fastembed (ONNX, local, ~67 MB one-time
model download for the default BAAI/bge-small-en-v1.5); tests inject a
deterministic embedder with the same duck-typed surface:

    dim: int, model_name: str, embed_documents(texts), embed_query(text),
    ready() -> bool, wait_ready(timeout) -> None (raises if unavailable)

The fastembed import lives ONLY in this module, behind `FastEmbedEmbedder`.
"""

from __future__ import annotations

import logging
import sys
import threading
import time
from typing import List, Optional

logger = logging.getLogger("mnestic-mcp")

DEFAULT_MODEL = "BAAI/bge-small-en-v1.5"


def model_dim(model_name: str) -> int:
    """Resolve a model's embedding dimension from fastembed's offline registry
    (no download). Raises for unknown models with the supported list."""
    from fastembed import TextEmbedding

    for entry in TextEmbedding.list_supported_models():
        if entry["model"].lower() == model_name.lower():
            return int(entry["dim"])
    known = ", ".join(sorted(e["model"] for e in TextEmbedding.list_supported_models())[:10])
    raise ValueError(
        f"unknown fastembed model {model_name!r}. First supported models: {known}, ..."
    )


class FastEmbedEmbedder:
    """Lazy/background-eager fastembed wrapper.

    `start_warmup()` loads (and on first run downloads) the model on a daemon
    thread so the MCP server answers `initialize` instantly; embedding tools
    call `wait_ready()` and get a crisp, actionable error if the model could
    not be fetched (keyword search never needs it)."""

    def __init__(self, model_name: str = DEFAULT_MODEL, cache_dir: Optional[str] = None) -> None:
        self.model_name = model_name
        self.cache_dir = cache_dir
        self.dim = model_dim(model_name)
        self._model = None
        self._lock = threading.Lock()
        self._ready = threading.Event()
        self._error: Optional[BaseException] = None
        self._warmup_thread: Optional[threading.Thread] = None

    def ready(self) -> bool:
        return self._ready.is_set() and self._error is None

    def start_warmup(self) -> None:
        if self._warmup_thread is not None or self._ready.is_set():
            return
        thread = threading.Thread(target=self._load, name="mnestic-mcp-embed-warmup", daemon=True)
        self._warmup_thread = thread
        thread.start()

    def _load(self) -> None:
        try:
            from fastembed import TextEmbedding

            started = time.monotonic()
            logger.info(
                "loading embedding model %s (first run downloads ~tens of MB, once, to %s)...",
                self.model_name,
                self.cache_dir or "the fastembed cache",
            )
            model = TextEmbedding(model_name=self.model_name, cache_dir=self.cache_dir)
            next(iter(model.embed(["warmup"])))  # force ONNX session init
            with self._lock:
                self._model = model
            logger.info("embedding model ready (%.1fs)", time.monotonic() - started)
        except BaseException as e:  # noqa: BLE001 - captured and re-raised on use
            self._error = e
            logger.error("embedding model failed to load: %s", e)
        finally:
            self._ready.set()

    def wait_ready(self, timeout: float = 120.0) -> None:
        self.start_warmup()
        if not self._ready.wait(timeout):
            raise RuntimeError(
                f"the embedding model ({self.model_name}) is still downloading/loading. "
                "Keyword search, list_recent, recall_as_of and stats work now; retry "
                "semantic search shortly, or pre-download with `mnestic-mcp --download-model`."
            )
        if self._error is not None:
            raise RuntimeError(
                f"the embedding model ({self.model_name}) could not be loaded: {self._error}. "
                "Keyword search still works. Check network access (HuggingFace Hub) and disk "
                "space, then retry or run `mnestic-mcp --download-model`."
            )

    def embed_documents(self, texts: List[str]) -> List[List[float]]:
        self.wait_ready()
        with self._lock:
            return [[float(x) for x in v] for v in self._model.embed(list(texts))]

    def embed_query(self, text: str) -> List[float]:
        self.wait_ready()
        with self._lock:
            return [float(x) for x in next(iter(self._model.query_embed(text)))]


def configure_stderr_logging() -> None:
    """stdout is the MCP wire — everything human-facing goes to stderr."""
    logging.basicConfig(
        stream=sys.stderr,
        level=logging.INFO,
        format="mnestic-mcp: %(message)s",
    )
    if not sys.stderr.isatty():
        import os

        os.environ.setdefault("HF_HUB_DISABLE_PROGRESS_BARS", "1")
