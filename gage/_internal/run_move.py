# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import os
import threading
import time

import ulid

from .file_util import ensure_dir
from .file_util import touch

__all__ = [
    "ACTIVE_CONTAINER",
    "log_run_move",
    "make_move_id",
    "run_move_dir",
    "run_container",
]

ACTIVE_CONTAINER = "active"


def log_run_move(run: Run, container: str):
    move_dir = run_move_dir(run)
    ensure_dir(move_dir)
    marker = os.path.join(move_dir, f"{make_move_id()}-{container}")
    touch(marker)
    return marker


def run_move_dir(run: Run):
    run_parent_dir = os.path.dirname(run.meta_dir)
    return os.path.join(run_parent_dir, f"{run.id}.move")


__last_ts = None
__last_ts_lock = threading.Lock()


def _now_ms():
    ts = time.time_ns() // 1000000  # milliseconds, used by ULID
    with __last_ts_lock:
        if __last_ts is not None and __last_ts >= ts:
            ts = __last_ts + 1
        globals()["__last_ts"] = ts
    return ts


def make_move_id(timestamp_ms: int = 0):
    timestamp_ms = timestamp_ms or _now_ms()
    return str(ulid.ULID.from_timestamp(timestamp_ms).to_uuid4())


def run_container(run: Run):
    move_dir = run_move_dir(run)
    try:
        names = os.listdir(move_dir)
    except FileNotFoundError:
        return "active"
    else:
        for name in sorted(names, reverse=True):
            try:
                return _container_for_marker(name)
            except ValueError:
                pass
        return "active"


def _container_for_marker(name: str):
    if len(name) < 37:
        raise ValueError(f"Unexpected run move marker file: {name}")
    return name[37:]
