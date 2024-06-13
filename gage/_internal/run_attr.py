# SPDX-License-Identifier: Apache-2.0

from typing import *

import datetime
import logging
import os

from .types import *

from .run_meta import read_opref

from . import attr_log
from . import run_meta
from . import util

__all__ = [
    "CORE_ATTRS",
    "run_config",
    "run_attr",
    "run_label",
    "run_opref",
    "run_project_dir",
    "run_project_ref",
    "run_status",
    "run_summary",
    "run_timestamp",
    "run_user_attrs",
    "run_user_dir",
]

log = logging.getLogger(__name__)


def run_status(run: Run):
    return cast(
        RunStatus,
        util.find_apply(
            [
                _exit_status,
                _running_status,
                _staged_status,
                _pending_status,
            ],
            run,
        ),
    )


def _exit_status(run: Run) -> Literal["completed", "terminated", "error"] | None:
    exit_code = run_attr(run, "exit_code", None)
    if exit_code is None:
        return None
    elif exit_code == 0:
        return "completed"
    elif exit_code < 0:
        return "terminated"
    else:
        return "error"


def _running_status(run: Run) -> Literal["running", "terminated"] | None:
    try:
        lock_str = run_meta.read_proc_lock(run)
    except FileNotFoundError:
        return None
    except Exception as e:
        log.warning("Error reading process status from \"%s\": %s", run.meta_dir, e)
        return None
    else:
        return "running" if _is_active_lock(lock_str) else "terminated"


def _is_active_lock(lock: str):
    # TODO: read lock = should have PID + some process hints to verify
    # PID belongs to expected run - for now assume valid
    return True


def _staged_status(run: Run) -> Literal["staged"] | None:
    return "staged" if run_meta.meta_file_exists(run, "staged") else None


def _pending_status(run: Run) -> Literal["pending", "unknown"] | None:
    return "pending" if run_meta.meta_file_exists(run, "initialized") else "unknown"


_RAISE = object()
_UNREAD = object()


def run_attr(run: Run, name: str, default: Any = _RAISE):
    """Returns a run attribute or default if attribute can't be read.

    Attributes may be read from the run meta directory or from the run
    itself depending on the attribute.

    Attribute results are alway cached. To re-read a run attribute from
    disk, read the attribute from a new run.
    """
    cache_name = f"_attr_{name}"
    try:
        return run._cache[cache_name]
    except KeyError:
        try:
            reader = cast(Callable[[Any, str, Any], Any], _ATTR_READERS[name])
        except KeyError:
            raise AttributeError(name) from None
        else:
            val = reader(run, name, _UNREAD)
            if val is _UNREAD:
                if default is _RAISE:
                    raise AttributeError(name) from None
                return default
            run._cache[cache_name] = val
            return val


def run_timestamp(run: Run, name: RunTimestamp, default: Any = None):
    try:
        with run_meta.open_meta_file(run, name) as f:
            timestamp_str = f.read()
    except FileNotFoundError:
        return default
    else:
        try:
            timestamp_int = int(timestamp_str.rstrip())
        except ValueError:
            log.warning("Invalid timestamp '%s' in \"%s\"", name, run.meta_dir)
            return default
        else:
            return datetime.datetime.fromtimestamp(timestamp_int / 1000000)


def _run_dir_reader(run: Run, name: str, default: Any = None):
    return run.run_dir


def _run_adaptive_timestamp_reader(run: Run, name: str, default: Any = None):
    # Ignore requested name - assumed to be 'timestamp'
    for name in ("started", "staged", "initialized"):
        val = run_timestamp(run, name, _UNREAD)
        if val is not _UNREAD:
            return val
    return default


def _run_exit_code_reader(run: Run, name: str, default: Any = None):
    try:
        exit_str = run_meta.read_proc_exit(run)
    except FileNotFoundError:
        return default
    except Exception as e:
        log.warning("Error reading exit status in \"%s\": %s", run.meta_dir, e)
        return default
    else:
        try:
            return int(exit_str)
        except ValueError:
            log.warning("Invalid exit status in \"%s\": %s", run.meta_dir, exit_str)
            return default


_ATTR_READERS = {
    "id": getattr,
    "name": getattr,
    "dir": _run_dir_reader,
    "staged": run_timestamp,
    "started": run_timestamp,
    "stopped": run_timestamp,
    "timestamp": _run_adaptive_timestamp_reader,
    "exit_code": _run_exit_code_reader,
}

CORE_ATTRS = list(_ATTR_READERS)


def run_summary(run: Run) -> RunSummary:
    try:
        data = run_meta.read_summary(run)
    except FileNotFoundError:
        return RunSummary({})
    else:
        return RunSummary(data)


def run_label(run: Run) -> str | None:
    return run_user_attrs(run).get("label") or run_summary(run).get_run_attrs().get(
        "label"
    )


def run_opref(run: Run):
    return read_opref(run)


def run_config(run: Run) -> RunConfig:
    try:
        return run_meta.read_config(run)
    except FileNotFoundError:
        return cast(RunConfig, {})


def run_user_attrs(run: Run) -> dict[str, Any]:
    attrs_dir = run_user_dir(run)
    if not os.path.exists(attrs_dir):
        return {}
    return attr_log.get_attrs(attrs_dir)


def run_user_dir(run: Run):
    return _run_other_dir(run, "user")


def _run_other_dir(run: Run, name: str):
    if run.run_dir.endswith(".deleted"):
        return "".join([run.run_dir[:-8], ".", name, ".deleted"])
    return "".join([run.run_dir, ".", name])


def run_project_ref(run: Run):
    return _run_other_dir(run, "project")


def run_project_dir(run: Run):
    ref_filename = run_project_ref(run)
    try:
        f = open(ref_filename)
    except FileNotFoundError:
        return None
    except Exception as e:
        log.warning("Error reading project ref in \"%s\": %s", ref_filename, e)
        return None
    else:
        try:
            with f:
                uri = f.read().rstrip()
        except Exception as e:
            log.warning("Error reading project ref in \"%s\": %s", ref_filename, e)
            return None
        else:
            if not uri.startswith("file:"):
                log.warning("Unexpected project ref encoding in \"%s\"", ref_filename)
                return None
            return _abs_project_dir(uri[5:], run)


def _abs_project_dir(project_ref_path: str, run: Run):
    return os.path.realpath(
        os.path.join(os.path.dirname(run.run_dir), project_ref_path)
    )
