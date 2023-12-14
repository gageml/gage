# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import json
import os
import shutil
import stat
import threading
import time

import ulid

__all__ = [
    "LogEntry",
    "UNKNOWN_AUTHOR",
    "get_attrs",
    "get_attrs_by_author",
    "log_attrs",
    "make_log_id",
    "merge_attrs",
    "now_ms",
]

UNKNOWN_AUTHOR = "__unknown__"


class LogEntry(NamedTuple):
    author: str
    set: dict[str, Any]
    delete: list[str]


def get_attrs(src_dir: str):
    attrs: dict[str, Any] = {}
    for log_entry in _iter_log_entries(src_dir):
        _apply_log_entry(log_entry, attrs)
    return attrs


def _iter_log_entries(src_dir: str):
    for name in sorted(os.listdir(src_dir)):
        if not name.endswith(".json"):
            continue
        filename = os.path.join(src_dir, name)
        yield _read_log_entry(filename)


def _apply_log_entry(log_entry: LogEntry, attrs: dict[str, Any]):
    attrs.update(log_entry.set)
    for name in log_entry.delete:
        attrs.pop(name, None)


def get_attrs_by_author(src_dir: str, author: str = ""):
    attrs_by_author: dict[str, dict[str, Any]] = {}
    for log_entry in _iter_log_entries(src_dir):
        if author and log_entry.author != author:
            continue
        attrs = attrs_by_author.setdefault(log_entry.author, {})
        _apply_log_entry(log_entry, attrs)
    return attrs_by_author


def _read_log_entry(filename: str):
    with open(filename) as f:
        data = json.load(f)
        return LogEntry(
            data.get("author", UNKNOWN_AUTHOR),
            data.get("set", {}),
            data.get("delete", []),
        )


def log_attrs(
    dest_dir: str,
    author: str,
    set: dict[str, Any],
    delete: list[str] | None = None,
):
    timestamp = now_ms()
    log_id = make_log_id(timestamp)
    log_filename = os.path.join(dest_dir, log_id + ".json")
    log_entry = LogEntry(author, set, delete or [])
    _write_log_entry(log_entry, log_filename)


def _write_log_entry(log_entry: LogEntry, filename: str):
    data = {
        "author": log_entry.author,
        "set": log_entry.set,
        "delete": log_entry.delete,
    }
    with open(filename, "w") as f:
        json.dump(data, f)
    _set_readonly(filename)


__last_ts = None
__last_ts_lock = threading.Lock()


def now_ms():
    ts = time.time_ns() // 1000000  # milliseconds, used by ULID
    with __last_ts_lock:
        if __last_ts is not None and __last_ts >= ts:
            ts = __last_ts + 1
        globals()["__last_ts"] = ts
    return ts


def make_log_id(timestamp_ms: int = 0):
    timestamp_ms = timestamp_ms or now_ms()
    return str(ulid.ULID.from_timestamp(timestamp_ms).to_uuid4())


def merge_attrs(src_dir: str, dest_dir: str):
    """Never-replace, non-recursive file copy from src to dest."""
    for name in os.listdir(src_dir):
        if not name.endswith(".json"):
            continue
        dest_filename = os.path.join(dest_dir, name)
        if os.path.exists(dest_filename):
            continue
        shutil.copyfile(os.path.join(src_dir, name), dest_filename)


def _set_readonly(filename: str):
    mode = stat.S_IREAD | stat.S_IRGRP | stat.S_IROTH
    os.chmod(filename, mode)
