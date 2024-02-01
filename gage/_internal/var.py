# SPDX-License-Identifier: Apache-2.0

from typing import *
from .types import *

import functools
import logging
import os

from .run_util import run_attr
from .run_util import run_for_meta_dir
from .run_util import run_user_dir
from .run_util import run_project_ref

from .file_util import ensure_safe_delete_tree
from .file_util import safe_delete_tree

from .project_util import find_project_dir

from .gagefile import GageFileLoadError
from .gagefile import gagefile_for_dir

__all__ = [
    "delete_runs",
    "list_runs",
    "purge_runs",
    "set_runs_dir",
    "restore_runs",
    "runs_dir",
]

log = logging.getLogger(__name__)


# =================================================================
# Runs dir
# =================================================================


def runs_dir():
    return os.path.expanduser(
        os.getenv("GAGE_RUNS")
        or os.getenv("RUNS_DIR")
        or _project_runs_dir()
        or _system_default_runs_dir()
    )


def _project_runs_dir():
    project_dir = find_project_dir()
    if not project_dir:
        return None
    try:
        gf = gagefile_for_dir(project_dir)
    except (FileNotFoundError, GageFileLoadError) as e:
        log.debug("error reading Gage file in %s: %s", project_dir, e)
        return _project_default_runs_dir(project_dir)
    else:
        return _project_configured_runs_dir(gf, project_dir)


def _project_default_runs_dir(project_dir: str):
    return os.path.join(project_dir, ".gage", "runs")


def _project_configured_runs_dir(gf: GageFile, project_dir: str):
    configured = gf.get_runs_dir()
    return (
        os.path.join(project_dir, configured)
        if configured
        else _project_default_runs_dir(project_dir)
    )


def set_runs_dir(dirname: str):
    os.environ["GAGE_RUNS"] = dirname


def _system_default_runs_dir():
    return os.path.join("~", ".gage", "runs")


# =================================================================
# List runs
# =================================================================


def list_runs(
    root: str | None = None,
    filter: RunFilter | None = None,
    sort: list[str] | None = None,
    deleted: bool = False,
):
    root = root or runs_dir()
    log.debug("Getting runs from %s", root)
    filter = filter or _all_runs_filter
    runs_iter = _iter_runs(root, deleted)
    runs = [run for run in runs_iter if filter(run)] if filter else list(runs_iter)
    if not sort:
        return runs
    return sorted(runs, key=_run_sort_key(sort))


def _all_runs_filter(run: Run):
    return True


def _iter_runs(root: str, deleted: bool = False):
    try:
        names = os.listdir(root)
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
    for src in _existing_run_sources(run):
        _handle_delete_run_source(src, permanent)


def _handle_delete_run_source(src: str, permanent: bool):
    if permanent:
        safe_delete_tree(src)
    else:
        deleted_src_target = src + ".deleted"
        ensure_safe_delete_tree(deleted_src_target)
        os.rename(src, deleted_src_target)


def _existing_run_sources(run: Run):
    return [path for path in _canonical_run_sources(run) if os.path.exists(path)]


def _canonical_run_sources(run: Run):
    return [
        run.meta_dir,
        run.run_dir,
        run_user_dir(run),
        run_project_ref(run),
    ]


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
    for src, target in _safe_restorable(run):
        assert not os.path.exists(target)
        os.rename(src, target)


def _safe_restorable(run: Run):
    sources = _canonical_run_sources(run)
    targets = _restore_targets(sources)
    for path in targets:
        if os.path.exists(path):
            raise FileExistsError(path)
    return [
        (src, target)
        for src, target in zip(sources, targets)  # \
        if os.path.exists(src)
    ]


def _restore_targets(sources: list[str]):
    assert all(src.endswith(".deleted") for src in sources), sources
    return [src[:-8] for src in sources]


# =================================================================
# Purge runs
# =================================================================


def purge_runs(runs: list[Run]):
    removed = delete_runs(runs, permanent=True)
    return removed
