# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import json
import logging
import os
import threading
import time
import uuid

import ulid

from . import var

from .file_util import ensure_dir
from .file_util import safe_list_dir
from .file_util import touch

__all__ = [
    "ArchiveNotFoundError",
    "archive_for_name",
    "iter_archives",
    "make_archive",
    "now_timestamp",
    "delete_archive",
    "update_archive",
]

log = logging.getLogger(__name__)


class ArchiveNotFoundError(ValueError):
    pass


def make_archive(name: str, runs_dir: str | None = None):
    runs_dir = runs_dir or var.runs_dir()
    archive_id = str(uuid.uuid4())
    data = _archive_data(archive_id, name)
    filename = _archive_timestamp_filename(runs_dir, archive_id, ".json")
    ensure_dir(os.path.dirname(filename))
    with open(filename, "w") as f:
        json.dump(data, f)
    return ArchiveDef(filename, data)


def _archive_data(id: str, name: str) -> dict[str, Any]:
    return {
        "id": id,
        "name": name,
        "date": now_timestamp(),
    }


def now_timestamp():
    return int(time.time())


def _archive_timestamp_filename(runs_dir: str, archive_id: str, ext: str):
    return os.path.join(_archives_dir(runs_dir), archive_id, _archive_timestamp() + ext)


def _archives_dir(runs_dir: str):
    return os.path.join(runs_dir, ".archives")


__last_ts = None
__last_ts_lock = threading.Lock()


def _archive_timestamp():
    ts = time.time_ns() // 1000000  # milliseconds, used by ULID
    with __last_ts_lock:
        if __last_ts is not None and __last_ts >= ts:
            ts = __last_ts + 1
        globals()["__last_ts"] = ts
    return str(ulid.ULID.from_timestamp(ts).to_uuid4())


def iter_archives(runs_dir: str | None = None):
    runs_dir = runs_dir or var.runs_dir()
    for filename in _iter_active_archives(runs_dir):
        try:
            with open(filename) as f:
                yield ArchiveDef(filename, json.load(f))
        except Exception as e:
            log.warning("Error loading archive from \"%s\": %s", filename, e)


def _iter_active_archives(runs_dir: str):
    archives_dir = _archives_dir(runs_dir)
    for archive_id in safe_list_dir(archives_dir):
        archive_dir = os.path.join(archives_dir, archive_id)
        for name in sorted(safe_list_dir(archive_dir), reverse=True):
            if name.endswith(".deleted"):
                break
            if name.endswith(".json"):
                yield os.path.join(archive_dir, name)
                break


def archive_for_name(name: str, runs_dir: str | None = None):
    for archive in iter_archives(runs_dir):
        if archive.get_name() == name:
            return archive
    raise ArchiveNotFoundError(name)


def delete_archive(archive_id: str, runs_dir: str | None = None):
    runs_dir = runs_dir or var.runs_dir()
    filename = _archive_timestamp_filename(runs_dir, archive_id, ".deleted")
    touch(filename)


def update_archive(
    archive: ArchiveDef,
    name: str | None = None,
    last_archived: int | None = None,
    runs_dir: str | None = None,
):
    runs_dir = runs_dir or var.runs_dir()
    data = {**archive.as_json()}
    if name:
        data["name"] = name
    if last_archived:
        data["date"] = last_archived
    filename = _archive_timestamp_filename(runs_dir, archive.get_id(), ".json")
    with open(filename, "w") as f:
        json.dump(data, f)
    return ArchiveDef(archive.filename, data)
