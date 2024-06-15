# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import functools
import logging
import os

from .run_attr import run_attr

from .run_move import ACTIVE_CONTAINER
from .run_move import log_run_move
from .run_move import run_container

from .run_util import run_for_meta_dir

from .file_util import safe_delete_tree

from .project_util import find_project_dir

from .gagefile import GageFileLoadError
from .gagefile import gagefile_for_dir

__all__ = [
    "delete_run",
    "delete_runs",
    "list_runs",
    "move_run",
    "move_runs",
    "set_runs_dir",
    "runs_dir",
]

log = logging.getLogger(__name__)

TRASH = "trash"
ACTIVE = ACTIVE_CONTAINER


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


def _system_default_runs_dir():
    return os.path.join("~", ".gage", "runs")


def set_runs_dir(dirname: str):
    os.environ["GAGE_RUNS"] = dirname


# =================================================================
# Archives dir
# =================================================================


def archives_dir():
    return os.path.expanduser(
        os.getenv("GAGE_ARCHIVES")
        or _project_archives_dir()
        or _system_default_archives_dir()
    )


def _project_archives_dir():
    project_dir = find_project_dir()
    if not project_dir:
        return None
    try:
        gf = gagefile_for_dir(project_dir)
    except (FileNotFoundError, GageFileLoadError) as e:
        log.debug("error reading Gage file in %s: %s", project_dir, e)
        return _project_default_archives_dir(project_dir)
    else:
        return _project_configured_archives_dir(gf, project_dir)


def _project_default_archives_dir(project_dir: str):
    return os.path.join(project_dir, ".gage", "archives")


def _project_configured_archives_dir(gf: GageFile, project_dir: str):
    configured = gf.get_archives_dir()
    return (
        os.path.join(project_dir, configured)
        if configured
        else _project_default_archives_dir(project_dir)
    )


def _system_default_archives_dir():
    return os.path.join("~", ".gage", "archives")


def set_archives_dir(dirname: str):
    os.environ["GAGE_ARCHIVES"] = dirname


# =================================================================
# List runs
# =================================================================


def list_runs(
    root: str | None = None,
    filter: RunFilter | None = None,
    sort: list[str] | None = None,
    container: str = ACTIVE,
):
    root = root or runs_dir()
    log.debug("Getting runs from %s", root)
    filter = filter or _all_runs_filter
    runs_iter = _iter_runs(root, container)
    runs = [run for run in runs_iter if filter(run)] if filter else list(runs_iter)
    if not sort:
        return runs
    return sorted(runs, key=_run_sort_key(sort))


def _all_runs_filter(run: Run):
    return True


def _iter_runs(root: str, container: str):
    try:
        names = os.listdir(root)
    except OSError:
        pass
    else:
        for name in names:
            if not _is_meta_name(name):
                continue
            meta_dir = os.path.join(root, name)
            run = run_for_meta_dir(meta_dir)
            if not run:
                continue
            if _match_run_container(run, container):
                yield run


def _is_meta_name(name: str):
    return name.endswith(".meta") or name.endswith(".meta.zip")


def _match_run_container(run: Run, container: str):
    return run_container(run) == container


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
# Move runs
# =================================================================


def move_runs(runs: list[Run], container: str):
    for run in runs:
        move_run(run, container)


def move_run(run: Run, container: str):
    if not os.path.exists(run.meta_dir):
        raise FileNotFoundError(run.meta_dir)
    log_run_move(run, container)


def restore_runs(runs: list[Run]):
    for run in runs:
        restore_run(run)


def restore_run(run: Run):
    move_run(run, ACTIVE_CONTAINER)


# =================================================================
# Delete runs
# =================================================================


def delete_runs(runs: list[Run]):
    for run in runs:
        delete_run(run)


def delete_run(run: Run):
    for path in _iter_run_sources_for_delete(run):
        log.debug("Permanently deleting run source: %s", path)
        safe_delete_tree(path)


def _iter_run_sources_for_delete(run: Run):
    # Yield meta dir first as this is how a run is defined on disk
    yielded_meta_dir_name = None
    if os.path.exists(run.meta_dir):
        yield run.meta_dir
        yielded_meta_dir_name = run.meta_dir
    run_parent_dir = os.path.dirname(run.meta_dir)
    for name in os.listdir(run_parent_dir):
        if name.startswith(run.id) and name != yielded_meta_dir_name:
            yield os.path.join(run_parent_dir, name)
