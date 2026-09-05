"""Configuración de logging para todo el pipeline."""

from __future__ import annotations

import logging
import sys


_FORMAT = "%(asctime)s %(levelname)-7s %(name)-22s %(message)s"
_DATEFMT = "%H:%M:%S"


class _ColorFormatter(logging.Formatter):
    COLORS = {
        logging.DEBUG: "\033[38;5;244m",
        logging.INFO: "\033[38;5;39m",
        logging.WARNING: "\033[38;5;214m",
        logging.ERROR: "\033[38;5;196m",
        logging.CRITICAL: "\033[48;5;196m\033[38;5;231m",
    }
    RESET = "\033[0m"

    def __init__(self, use_color: bool) -> None:
        super().__init__(_FORMAT, datefmt=_DATEFMT)
        self.use_color = use_color

    def format(self, record: logging.LogRecord) -> str:
        text = super().format(record)
        if not self.use_color:
            return text
        return f"{self.COLORS.get(record.levelno, '')}{text}{self.RESET}"


def setup_logging(verbose: bool = False) -> None:
    """Instala un handler en stderr. Idempotente."""
    root = logging.getLogger()
    if getattr(root, "_viralshorts_configured", False):
        return

    handler = logging.StreamHandler(sys.stderr)
    handler.setFormatter(_ColorFormatter(use_color=sys.stderr.isatty()))
    root.addHandler(handler)
    root.setLevel(logging.DEBUG if verbose else logging.INFO)

    # Estas librerías son muy ruidosas en DEBUG.
    for noisy in ("urllib3", "httpx", "httpcore", "anthropic", "faster_whisper"):
        logging.getLogger(noisy).setLevel(logging.WARNING)

    root._viralshorts_configured = True  # type: ignore[attr-defined]
