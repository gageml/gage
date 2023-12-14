# SPDX-License-Identifier: Apache-2.0

from typing import *

import logging
import logging.config
import os
import sys

from . import ansi_util

__all__ = [
    "init_logging",
    "current_settings",
]

__last_init_kw = {}

_isatty = sys.stderr.isatty()
_shell = os.getenv("SHELL")

DEBUG = logging.DEBUG
INFO = logging.INFO
WARN = logging.WARN
ERROR = logging.ERROR


class _FakeTTY:
    """Context manager for forcing _isatty to True - used for tests."""

    _saved = None

    def __enter__(self):
        self._saved = _isatty
        globals()["_isatty"] = True

    def __exit__(self, *exc: Any):
        assert self._saved is not None
        globals()["_isatty"] = self._saved


class _FakeShell:
    """Context manager for defining _shell as 'fake' - used for tests."""

    _saved = None

    def __enter__(self):
        self._saved = _shell
        globals()["_shell"] = "fake"

    def __exit__(self, *exc: Any):
        globals()["_shell"] = self._saved


class Formatter(logging.Formatter):
    def format(self, record: logging.LogRecord):
        return self._maybe_strip_ansi(
            self._color(super().format(record), record.levelno)
        )

    @staticmethod
    def _color(s: str, level: int):
        if not _isatty or not _shell:
            return s
        if level >= logging.ERROR:
            return f"\033[31m{s}\033[0m"
        if level >= logging.WARNING:
            return f"\033[33m{s}\033[0m"
        return s

    @staticmethod
    def _maybe_strip_ansi(s: str):
        if _isatty:
            return s
        return ansi_util.strip_ansi(s)


class ConsoleLogHandler(logging.StreamHandler):
    DEFAULT_FORMATS = {
        "_": "%(levelname)s: %(message)s",
        "DEBUG": "%(levelname)s: [%(name)s] %(message)s",
        "INFO": "%(message)s",
    }

    def __init__(self, formats: Optional[dict[str, str]] = None):
        super().__init__()
        formats = formats or self.DEFAULT_FORMATS
        self._formatters = {level: Formatter(fmt) for level, fmt in formats.items()}

    def format(self, record: logging.LogRecord):
        fmt = self._formatters.get(record.levelname) or self._formatters.get("_")
        if fmt:
            return fmt.format(record)
        return super().format(record)


def init_logging(level: int | None = None, formats: Optional[dict[str, str]] = None):
    level = _log_level_for_arg(level)
    console_handler = {
        "class": "gage._internal.log.ConsoleLogHandler",
        "formats": formats,
    }
    logging.config.dictConfig(
        {
            "version": 1,
            "handlers": {"console": console_handler},
            "root": {"level": level, "handlers": ["console"]},
            "disable_existing_loggers": False,
        }
    )
    globals()["__last_init_kw"] = {"level": level, "formats": formats}


def _log_level_for_arg(arg: Optional[int]):
    if arg is not None:
        return arg
    try:
        return int(os.environ["LOG_LEVEL"])
    except (KeyError, TypeError):
        return logging.INFO


def current_settings() -> dict[str, Any]:
    return __last_init_kw
