# SPDX-License-Identifier: Apache-2.0

from typing import *
from .types import *

import functools
import logging
import os

from . import sys_config

from .run_util import run_attr
from .run_util import run_for_meta_dir
from .run_util import run_user_dir
from .run_util import run_project_ref

from .file_util import ensure_safe_delete_tree
from .file_util import safe_delete_tree

__all__ = [
    "list_runs",
    "delete_runs",
    "purge_runs",
    "restore_runs",
]

log = logging.getLogger(__name__)


# =================================================================
# List runs
# =================================================================


RunFilter = Callable[[Run], bool]


def list_runs(
    root: str | None = None,
    filter: RunFilter | None = None,
    sort: list[str] | None = None,
    deleted: bool = False,
):
    root = root or sys_config.get_runs_home()
    filter = filter or _all_runs_filter
    runs = [run for run in _iter_runs(root, deleted) if filter(run)]
    if not sort:
        return runs
    return sorted(runs, key=_run_sort_key(sort))


def _all_runs_filter(run: Run):
    return True


def _iter_runs(root: str, deleted: bool = False):
    try:
        names = set(os.listdir(root))
    except OSError:
        pass
    else:
        for name in names:
            if not _is_meta_name(name, deleted):
                continue
            meta_dir = os.path.join(root, name)
            run = run_for_meta_dir(meta_dir)
            if run:
                yield run


def _is_meta_name(name: str, deleted: bool):
    return name.endswith(".meta.deleted") if deleted else name.endswith(".meta")


def _run_sort_key(sort: list[str]):
    def cmp(a: Run, b: Run):
        return _run_cmp(a, b, sort)

    return functools.cmp_to_key(cmp)


def _run_cmp(a: Run, b: Run, sort: list[str]):
    for attr in sort:
        attr_cmp = _run_attr_cmp(a, b, attr)
        if attr_cmp != 0:
            return attr_cmp
    return 0


def _run_attr_cmp(a: Run, b: Run, attr: str):
    if attr.startswith("-"):
        attr = attr[1:]
        rev = -1
    else:
        rev = 1
    x_val = run_attr(a, attr, None)
    if x_val is None:
        return -rev
    y_val = run_attr(b, attr, None)
    if y_val is None:
        return rev
    return rev * ((x_val > y_val) - (x_val < y_val))


# =================================================================
# Delete runs
# =================================================================


def delete_runs(runs: list[Run], permanent: bool = False):
    for run in runs:
        _delete_run(run, permanent)
    return runs


def _delete_run(run: Run, permanent: bool):
    if permanent:
        for src in _run_sources(run):
            _delete_tree(src)
    else:
        deleted_meta_dir = _deleted_meta_dir(run)
        ensure_safe_delete_tree(deleted_meta_dir)
        _move(run.meta_dir, deleted_meta_dir)


def _run_sources(run: Run):
    if os.path.exists(run.meta_dir):
        yield run.meta_dir
    if os.path.exists(run.run_dir):
        yield run.run_dir
    user_dir = run_user_dir(run)
    if os.path.exists(user_dir):
        yield user_dir
    project_ref = run_project_ref(run)
    if os.path.exists(project_ref):
        yield project_ref


def _deleted_meta_dir(run: Run):
    return run.meta_dir + ".deleted"


def _delete_tree(dirname: str):
    safe_delete_tree(dirname)


def _move(src: str, dest: str):
    os.rename(src, dest)


# =================================================================
# Restore runs
# =================================================================


def restore_runs(runs: list[Run]):
    restored: list[Run] = []
    for run in runs:
        try:
            _restore_run(run)
        except FileExistsError as e:
            log.warning("%s has already been restored", run.id)
        else:
            restored.append(run)
    return restored


def _restore_run(run: Run):
    assert run.meta_dir.endswith(".deleted")
    restored_meta_dir = run.meta_dir[:-8]
    if os.path.exists(restored_meta_dir):
        raise FileExistsError(restored_meta_dir)
    _move(run.meta_dir, restored_meta_dir)


# =================================================================
# Purge runs
# =================================================================


def purge_runs(runs: list[Run]):
    removed = delete_runs(runs, permanent=True)
    return removed
