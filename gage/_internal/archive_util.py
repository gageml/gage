# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import datetime
import os
import time

import ulid

from .file_util import make_dir
from .file_util import safe_list_dir
from .file_util import touch

from .var import archives_dir
from .var import list_runs

__all__ = [
    "ArchiveInfo",
    "archive_modified",
    "delete_archive",
    "find_archive_dir",
    "get_archive_name",
    "iter_archives",
    "list_archive_runs",
    "make_archive_dir",
    "make_archive_id",
    "set_archive_name",
    "touch_archive",
]

_DOT_ARCHIVE_SCHEMA = "1"


class ArchiveInfo(NamedTuple):
    name: str
    dirname: str


def make_archive_id():
    ts = time.time_ns() // 1000000  # milliseconds, used by ULID
    return str(ulid.ULID.from_timestamp(ts).to_uuid4())


def make_archive_dir(name: str):
    id = make_archive_id()
    dirname = os.path.join(archives_dir(), id)
    make_dir(dirname)
    set_archive_name(dirname, name)
    return dirname


def set_archive_name(dirname: str, name: str):
    with open(os.path.join(dirname, ".archive"), "w") as f:
        f.write(f"{_DOT_ARCHIVE_SCHEMA}{name}")


def find_archive_dir(name: str):
    for archive in iter_archives():
        if archive.name == name:
            return archive.dirname
    return None


def get_archive_name(dirname: str):
    dot_archive = os.path.join(dirname, ".archive")
    try:
        f = open(dot_archive)
    except FileNotFoundError:
        return None
    else:
        out = f.read().rstrip()
        if not out.startswith(_DOT_ARCHIVE_SCHEMA):
            return None
        return out[1:]


def iter_archives():
    basedir = archives_dir()
    for subdir in sorted(safe_list_dir(basedir)):
        dirname = os.path.join(basedir, subdir)
        name = get_archive_name(dirname)
        if not name:
            continue
        yield ArchiveInfo(name, dirname)


def archive_modified(dirname: str):
    dot_archive = os.path.join(dirname, ".archive")
    try:
        mtime = os.path.getmtime(dot_archive)
    except OSError:
        return None
    else:
        return datetime.datetime.fromtimestamp(mtime)


def touch_archive(dirname: str):
    touch(os.path.join(dirname, ".archive"))


def list_archive_runs(dirname: str):
    return list_runs(dirname, sort=["-timestamp"])


def delete_archive(dirname: str):
    try:
        os.remove(os.path.join(dirname, ".archive"))
    except FileNotFoundError:
        pass
