# SPDX-License-Identifier: Apache-2.0

from typing import *

from logging import Logger

from .types import *

import datetime
import io
import json
import logging
import os
import subprocess
import threading
import time
import uuid

from proquint import uint2quint

from . import channel
from . import sys_config
from . import run_config
from . import run_sourcecode
from . import run_output
from . import attr_log
from . import util

from .file_select import copy_files

from .file_util import ensure_dir
from .file_util import file_sha256
from .file_util import is_readonly
from .file_util import make_dir
from .file_util import set_readonly
from .file_util import write_file

from .opref_util import decode_opref
from .opref_util import encode_opref

from .sys_config import get_user

__all__ = [
    "META_SCHEMA",
    "RunManifest",
    "RunManifestEntry",
    "RunFileType",
    "apply_config",
    "associate_project",
    "finalize_run",
    "finalize_staged_run",
    "format_run_timestamp",
    "init_run_meta",
    "init_run_user_attrs",
    "log_user_attrs",
    "make_run_id",
    "make_run_timestamp",
    "make_run",
    "meta_config",
    "meta_opdef",
    "meta_opref",
    "open_run_output",
    "remove_associate_project",
    "run_attr",
    "run_meta_path",
    "run_name_for_id",
    "run_phase_channel",
    "run_project_dir",
    "run_project_ref",
    "run_status",
    "run_timestamp",
    "run_user_attrs",
    "run_user_dir",
    "stage_dependencies",
    "stage_run",
    "stage_runtime",
    "stage_sourcecode",
    "start_run",
]

META_SCHEMA = "1"

log = logging.getLogger(__name__)

run_phase_channel = channel.Channel()


# =================================================================
# Run status
# =================================================================


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
    if exit_code == 0:
        return "completed"
    elif exit_code < 0:
        return "terminated"
    return "error"


def _running_status(run: Run) -> Literal["running", "terminated"] | None:
    filename = _meta_proc_lock_filename(run)
    try:
        lock_str = open(filename).read().rstrip()
    except FileNotFoundError:
        return None
    except Exception as e:
        log.warning("Error reading process status in \"%s\": %s", filename, e)
        return None
    else:
        return "running" if _is_active_lock(lock_str) else "terminated"


def _is_active_lock(lock: str):
    # TODO: read lock = should have PID + some process hints to verify
    # PID belongs to expected run - for now assume valid
    return True


def _staged_status(run: Run) -> Literal["staged"] | None:
    filename = _meta_timestamp_filename(run, "staged")
    return "staged" if os.path.exists(filename) else None


def _pending_status(run: Run) -> Literal["pending", "unknown"] | None:
    filename = _meta_timestamp_filename(run, "initialized")
    return "pending" if os.path.exists(filename) else "unknown"


# =================================================================
# Run attrs
# =================================================================

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
    filename = _meta_timestamp_filename(run, name)
    try:
        timestamp_str = open(filename).read()
    except FileNotFoundError:
        return default
    else:
        try:
            timestamp_int = int(timestamp_str.rstrip())
        except ValueError:
            log.warning("Invalid run timestamp in \"%s\"", filename)
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
    filename = _meta_proc_exit_filename(run)
    try:
        exit_str = open(filename).read().rstrip()
    except FileNotFoundError:
        return default
    except Exception as e:
        log.warning("Error reading exit status in \"%s\": %s", filename, e)
        return default
    else:
        try:
            return int(exit_str)
        except ValueError:
            log.warning("Invalid exit status in \"%s\": %s", filename, exit_str)
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


# =================================================================
# Other run directories
# =================================================================


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
            return uri[5:]


def run_project_ref(run: Run):
    return run.run_dir + ".project"


def run_user_dir(run: Run):
    return run.run_dir + ".user"


# =================================================================
# Meta API
# =================================================================


def run_meta_path(run: Run, *path: str):
    return os.path.join(run.meta_dir, *path)


def meta_opref(run: Run) -> OpRef:
    with open(run_meta_path(run, "opref")) as f:
        return decode_opref(f.read())


def meta_opdef(run: Run) -> OpDef:
    opref = meta_opref(run)
    return OpDef(opref.op_name, _decode_json(_meta_opdef_filename(run)))


def _decode_json(filename: str):
    return json.load(open(filename))


def meta_config(run: Run) -> RunConfig:
    return _decode_json(run_meta_path(run, "config.json"))


def meta_opcmd(run: Run) -> OpCmd:
    return OpCmd(
        _decode_json(_meta_proc_cmd_filename(run)),
        _decode_json(_meta_proc_env_filename(run)),
    )


# =================================================================
# Meta filenames
# =================================================================


def _meta_id_filename(run: Run):
    return run_meta_path(run, "id")


def _meta_opref_filename(run_or_meta_dir: Run | str):
    if isinstance(run_or_meta_dir, Run):
        return run_meta_path(run_or_meta_dir, "opref")
    return os.path.join(run_or_meta_dir, "opref")


def _meta_opdef_filename(run: Run):
    return run_meta_path(run, "opdef.json")


def _meta_config_filename(run: Run):
    return run_meta_path(run, "config.json")


def _meta_proc_cmd_filename(run: Run):
    return run_meta_path(run, "proc", "cmd.json")


def _meta_proc_env_filename(run: Run):
    return run_meta_path(run, "proc", "env.json")


def _meta_schema_filename(run: Run):
    return run_meta_path(run, "__schema__")


def _meta_runner_log_filename(run: Run):
    return run_meta_path(run, "log", "runner")


def _meta_files_log_filename(run: Run):
    return run_meta_path(run, "log", "files")


def _meta_proc_exit_filename(run: Run):
    return run_meta_path(run, "proc", "exit")


def _meta_proc_lock_filename(run: Run):
    return run_meta_path(run, "proc", "lock")


def _meta_patched_filename(run: Run):
    return run_meta_path(run, "log", "patched")


def _meta_manifest_filename(run: Run):
    return run_meta_path(run, "manifest")


def _meta_timestamp_filename(run: Run, name: RunTimestamp):
    return run_meta_path(run, name)


# =================================================================
# Load run
# =================================================================


def run_for_meta_dir(meta_dir: str):
    try:
        opref = _load_opref(meta_dir)
    except (OSError, ValueError) as e:
        return None
    else:
        try:
            run_id = _load_run_id(meta_dir)
        except (OSError, ValueError):
            return None
        else:
            run_dir = _run_dir_for_meta_dir(meta_dir)
            run_name = run_name_for_id(run_id)
            return Run(run_id, opref, meta_dir, run_dir, run_name)


def _load_opref(meta_dir: str):
    filename = os.path.join(meta_dir, "opref")
    with open(filename) as f:
        return decode_opref(f.read())


def _load_run_id(meta_dir: str):
    filename = os.path.join(meta_dir, "id")
    with open(filename) as f:
        return f.read().rstrip()


def _run_dir_for_meta_dir(meta_dir: str):
    if meta_dir.endswith(".meta"):
        return meta_dir[:-5]
    if meta_dir.endswith(".meta.deleted"):
        return meta_dir[:-13]
    assert False, meta_dir


# =================================================================
# Make run
# =================================================================


def make_run(opref: OpRef, location: str | None = None, _id_time: int = 0):
    run_id = make_run_id(_id_time)
    location = location or sys_config.get_runs_home()
    run_dir = os.path.join(location, run_id)
    meta_dir = run_dir + ".meta"
    name = run_name_for_id(run_id)
    make_dir(meta_dir)
    _write_opref(opref, meta_dir)
    return Run(run_id, opref, meta_dir, run_dir, name)


def _write_opref(opref: OpRef, meta_dir: str):
    write_file(_meta_opref_filename(meta_dir), encode_opref(opref), readonly=True)


def make_run_id(_id_time: int = 0):
    if _id_time > 0:
        import ulid

        return str(ulid.ULID.from_timestamp(_id_time).to_uuid4())
    return str(uuid.uuid4())


def run_name_for_id(run_id: str) -> str:
    if len(run_id) < 8:
        raise ValueError(f"run ID is too short: {run_id!r}")
    return uint2quint(int(run_id[:8], 16))


__last_ts = None
__last_ts_lock = threading.Lock()


def make_run_timestamp():
    """Returns an integer in epoch microseconds use for run timestamps.

    Ensures that subsequent calls return increasing values.
    """
    ts = time.time_ns() // 1000
    with __last_ts_lock:
        if __last_ts is not None and __last_ts >= ts:
            ts = __last_ts + 1
        globals()["__last_ts"] = ts
    return ts


# =================================================================
# Meta init
# =================================================================


def init_run_meta(
    run: Run,
    opdef: OpDef,
    config: RunConfig,
    cmd: OpCmd,
    system_attrs: dict[str, Any] | None = None,
):
    _write_schema_file(run)
    log = _runner_log(run)
    _write_run_id(run, log)
    _write_opdef(opdef, run, log)
    _write_config(config, run, log)
    _write_proc_cmd(cmd, run, log)
    _write_proc_env(cmd, run, log)
    if system_attrs:
        _write_system_attrs(system_attrs, run, log)
    _write_timestamp("initialized", run, log)


def _write_schema_file(run: Run):
    write_file(_meta_schema_filename(run), str(META_SCHEMA), readonly=True)


def _write_run_id(run: Run, log: Logger):
    log.info("Writing meta id")
    write_file(_meta_id_filename(run), run.id, readonly=True)


def _write_opdef(opdef: OpDef, run: Run, log: Logger):
    log.info("Writing meta opdef")
    write_file(_meta_opdef_filename(run), _encode_json(opdef), readonly=True)


def _encode_json(val: Any):
    try:
        val = val.as_json()
    except AttributeError:
        pass
    return json.dumps(val, indent=2, sort_keys=True)


def _write_config(config: RunConfig, run: Run, log: Logger):
    log.info("Writing meta config")
    write_file(_meta_config_filename(run), _encode_json(config), readonly=True)


def _write_proc_cmd(cmd: OpCmd, run: Run, log: Logger):
    log.info("Writing meta proc cmd")
    filename = _meta_proc_cmd_filename(run)
    ensure_dir(os.path.dirname(filename))
    write_file(filename, _encode_json(cmd.args), readonly=True)


def _write_proc_env(cmd: OpCmd, run: Run, log: Logger):
    log.info("Writing meta proc env")
    filename = _meta_proc_env_filename(run)
    ensure_dir(os.path.dirname(filename))
    write_file(filename, _encode_json(cmd.env), readonly=True)


def _write_system_attrs(attrs: dict[str, Any], run: Run, log: Logger):
    _gen_write_attrs("sys", attrs, run, log)


def _gen_write_attrs(dir: str, attrs: dict[str, Any], run: Run, log: Logger):
    full_dir = run_meta_path(run, dir)
    ensure_dir(full_dir)
    for name in attrs:
        log.info("Writing meta %s/%s", dir, name)
        filename = os.path.join(full_dir, name + ".json")
        encoded = json.dumps(attrs[name])
        write_file(filename, encoded, readonly=True)


# =================================================================
# Associate project
# =================================================================


def associate_project(run: Run, project_dir: str):
    if not os.path.isabs(project_dir):
        raise ValueError(f"project_dir must be absolute: \"{project_dir}\"")
    ref_filename = run_project_ref(run)
    with open(ref_filename, "w") as f:
        f.write(f"file:{project_dir}")


def remove_associate_project(run: Run):
    ref_filename = run_project_ref(run)
    try:
        os.remove(ref_filename)
    except FileNotFoundError:
        pass


# =================================================================
# Run user attrs
# =================================================================


def run_user_attrs(run: Run) -> dict[str, Any]:
    attrs_dir = run_user_dir(run)
    if not os.path.exists(attrs_dir):
        return {}
    return attr_log.get_attrs(attrs_dir)


def init_run_user_attrs(run: Run, user_attrs: dict[str, Any]):
    if not user_attrs:
        return
    attrs_dir = run_user_dir(run)
    make_dir(attrs_dir)
    attr_log.log_attrs(attrs_dir, get_user(), user_attrs, [])


def log_user_attrs(run: Run, set: dict[str, Any], delete: list[str] | None = None):
    if not set and not delete:
        return
    attrs_dir = run_user_dir(run)
    if not os.path.exists(attrs_dir):
        if not set:
            return
        make_dir(attrs_dir)
    attr_log.log_attrs(attrs_dir, get_user(), set, delete)


# =================================================================
# Stage run
# =================================================================


def stage_run(run: Run, project_dir: str):
    stage_sourcecode(run, project_dir, _log_files=False)
    apply_config(run)
    stage_runtime(run, project_dir)
    stage_dependencies(run, project_dir)
    finalize_staged_run(run)


def stage_sourcecode(run: Run, project_dir: str, _log_files: bool = True):
    log = _runner_log(run)
    opdef = meta_opdef(run)
    run_phase_channel.notify("stage-sourcecode")
    _copy_sourcecode(run, project_dir, opdef, log)
    _stage_sourcecode_hook(run, project_dir, opdef, log)
    if _log_files:
        _apply_to_files_log(run, "s")


def _copy_sourcecode(run: Run, project_dir: str, opdef: OpDef, log: Logger):
    sourcecode = run_sourcecode.init(project_dir, opdef)
    log.info(f"Copying source code (see log/files): {sourcecode.patterns}")
    copy_files(project_dir, run.run_dir, sourcecode.paths)


def _stage_sourcecode_hook(run: Run, project_dir: str, opdef: OpDef, log: Logger):
    exec = opdef.get_exec().get_stage_sourcecode()
    if exec:
        _run_phase_exec(
            run,
            "stage-sourcecode",
            exec,
            _hook_env(run, project_dir),
            "10_sourcecode",
            log,
        )


def apply_config(run: Run):
    log = _runner_log(run)
    config = meta_config(run)
    opdef = meta_opdef(run)
    run_phase_channel.notify("stage-config")
    log.info("Applying configuration (see log/patched)")
    diffs = run_config.apply_config(config, opdef, run.run_dir)
    if diffs:
        _write_patched(run, diffs)
    _apply_to_files_log(run, "s")


def stage_runtime(run: Run, project_dir: str):
    log = _runner_log(run)
    opdef = meta_opdef(run)
    run_phase_channel.notify("stage-runtime")
    _stage_runtime_hook(run, project_dir, opdef, log)
    _apply_to_files_log(run, "r")


def _stage_runtime_hook(run: Run, project_dir: str, opdef: OpDef, log: Logger):
    exec = opdef.get_exec().get_stage_runtime()
    if exec:
        _run_phase_exec(
            run,
            "stage-runtime",
            exec,
            _hook_env(run, project_dir),
            "20_runtime",
            log,
        )


def stage_dependencies(run: Run, project_dir: str):
    log = _runner_log(run)
    opdef = meta_opdef(run)
    run_phase_channel.notify("stage-dependencies")
    _resolve_dependencies(run, project_dir, opdef, log)
    _stage_dependencies_hook(run, project_dir, opdef, log)
    _apply_to_files_log(run, "d")


def _resolve_dependencies(run: Run, project_dir: str, opdef: OpDef, log: Logger):
    for dep in opdef.get_dependencies():
        pass

    # TODO
    # dependencies = run_dependencies.init(project_dir, opdef)
    # log.info(f"Copying dependencies (see log/files): {dependencies.patterns}")
    # copy_files(project_dir, run.run_dir, dependencies.paths)


def _stage_dependencies_hook(run: Run, project_dir: str, opdef: OpDef, log: Logger):
    exec = opdef.get_exec().get_stage_dependencies()
    if exec:
        _run_phase_exec(
            run,
            "stage-dependencies",
            exec,
            _hook_env(run, project_dir),
            "30_dependencies",
            log,
        )


def finalize_staged_run(run: Run):
    log = _runner_log(run)
    _write_staged_files_manifest(run, log)
    _write_timestamp("staged", run, log)


def _write_staged_files_manifest(run: Run, log: Logger):
    log.info("Finalizing staged files (see manifest)")
    m = RunManifest(run, "w")
    with m:
        for type, path in _reduce_files_log(run):
            filename = os.path.join(run.run_dir, path)
            if not os.path.islink(filename):
                set_readonly(filename)
            digest = file_sha256(filename)
            m.add(type, digest, path)


def _reduce_files_log(run: Run):
    paths: Dict[str, RunFileType] = {}
    for event, type, modified, path in _iter_files_log(run):
        if event == "a":
            paths[path] = type
        elif event == "d":
            paths.pop(path, None)
    for path, type in paths.items():
        yield type, path


# =================================================================
# Start / finalize
# =================================================================


def start_run(run: Run):
    log = _runner_log(run)
    cmd = meta_opcmd(run)
    env = {**cmd.env, **os.environ}
    shell = isinstance(cmd.args, str)
    _write_timestamp("started", run, log)
    log.info(f"Starting run process: {cmd.args}")
    p = subprocess.Popen(
        cmd.args,
        env=env,
        cwd=run.run_dir,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        shell=shell,
    )
    _write_proc_lock(p, run, log)
    return p


def _write_proc_lock(proc: subprocess.Popen[bytes], run: Run, log: Logger):
    log.info("Writing meta proc/lock")
    filename = _meta_proc_lock_filename(run)
    ensure_dir(os.path.dirname(filename))
    write_file(filename, str(proc.pid), readonly=True)


def open_run_output(
    run: Run,
    p: subprocess.Popen[bytes],
    out_fileno: int | None = None,
    err_fileno: int | None = None,
    output_cb: run_output.OutputCallback | None = None,
):
    ensure_dir(run_meta_path(run, "output"))
    output_filename = run_meta_path(run, "output", "40_run")
    output = run_output.RunOutput(
        output_filename,
        out_fileno,
        err_fileno,
        output_cb,
    )
    output.open(p)
    return output


def finalize_run(run: Run, exit_code: int):
    log = _runner_log(run)
    opdef = meta_opdef(run)
    run_phase_channel.notify("finalize")
    ensure_dir(run.run_dir)
    _finalize_run_output(run)
    _write_timestamp("stopped", run, log)
    _write_exit_code(exit_code, run, log)
    _delete_proc_lock(run, log)
    _finalize_run_hook(run, opdef, log)
    _apply_to_files_log(run, "g")
    _finalize_files_log(run)
    _write_run_files_manifest(run, log)
    _finalize_runner_log(run)


def _finalize_run_output(run: Run):
    output_filename = run_meta_path(run, "output", "40_run")
    if os.path.exists(output_filename):
        set_readonly(output_filename)
    index_filename = run_meta_path(run, "output", "40_run.index")
    if os.path.exists(index_filename):
        set_readonly(index_filename)


def _write_exit_code(exit_code: int, run: Run, log: Logger):
    log.info("Writing meta proc/exit")
    filename = _meta_proc_exit_filename(run)
    ensure_dir(os.path.dirname(filename))
    write_file(filename, str(exit_code), readonly=True)


def _delete_proc_lock(run: Run, log: Logger):
    log.info("Deleting meta proc/lock")
    filename = _meta_proc_lock_filename(run)
    try:
        os.remove(filename)
    except OSError as e:
        log.info(f"Error deleting proc/lock: {e}")


def _finalize_run_hook(run: Run, opdef: OpDef, log: Logger):
    exec = opdef.get_exec().get_finalize_run()
    if exec:
        _run_phase_exec(
            run,
            "finalize-run",
            exec,
            _hook_env(run),
            "50_finalize",
            log,
        )


def _write_run_files_manifest(run: Run, log: Logger):
    log.info("Finalizing run files (see manifest)")
    index = _init_manifest_index(run)
    m = RunManifest(run, "w")
    with m:
        for type, path in _reduce_files_log(run):
            filename = os.path.join(run.run_dir, path)
            if not is_readonly(filename) and not os.path.islink(filename):
                set_readonly(filename)
            digest = file_sha256(filename)
            _maybe_log_file_changed(path, digest, index, log)
            m.add(type, digest, path)
    set_readonly(m.filename)


def _maybe_log_file_changed(
    path: str,
    digest: str,
    index: dict[str, str],
    log: Logger,
):
    try:
        orig_digest = index[path]
    except KeyError:
        pass
    else:
        if digest != orig_digest:
            log.info(f"File \"{path}\" was modified during the run")


def _finalize_files_log(run: Run):
    filename = _meta_files_log_filename(run)
    set_readonly(filename)


def _finalize_runner_log(run: Run):
    filename = _meta_runner_log_filename(run)
    set_readonly(filename)


# =================================================================
# Log support
# =================================================================

LoggedFileEvent = Literal[
    "a",  # added
    "d",  # deleted
    "m",  # modified
]

RunFileType = Literal[
    "s",  # source code
    "d",  # deleted
    "r",  # runtime
    "g",  # generated
]


def _runner_log(run: Run):
    filename = _meta_runner_log_filename(run)
    ensure_dir(os.path.dirname(filename))
    log = logging.Logger("runner")
    handler = logging.FileHandler(filename)
    log.addHandler(handler)
    formatter = logging.Formatter("%(asctime)s %(message)s", "%Y-%m-%dT%H:%M:%S%z")
    handler.setFormatter(formatter)
    return log


def _run_meta_schema(run: Run):
    with open(_meta_schema_filename(run)) as f:
        return f.read().rstrip()


def _write_timestamp(name: RunTimestamp, run: Run, log: Logger):
    log.info(f"Writing meta {name}")
    filename = run_meta_path(run, name)
    timestamp = make_run_timestamp()
    write_file(filename, str(timestamp), readonly=True)


def _apply_to_files_log(run: Run, type: RunFileType):
    pre_files = _init_pre_files_index(run)
    seen = set()
    with _open_files_log(run) as f:
        for entry in _iter_run_files(run):
            relpath = os.path.relpath(entry.path, run.run_dir)
            seen.add(relpath)
            modified = int(entry.stat().st_mtime * 1_000_000)
            pre_modified = pre_files.get(relpath)
            if modified == pre_modified:
                continue
            event = "a" if pre_modified is None else "m"
            encoded = _encode_logged_file(LoggedFile(event, type, modified, relpath))
            f.write(encoded)
        for path in pre_files:
            if path not in seen:
                encoded = _encode_logged_file(LoggedFile("d", type, None, path))
                f.write(encoded)


def _iter_run_files(run: Run):
    return sorted(_scan_files(run.run_dir), key=lambda entry: entry.path)


def _scan_files(dir: str) -> Generator[os.DirEntry[str], Any, None]:
    try:
        scanner = os.scandir(dir)
    except FileNotFoundError:
        return
    for entry in scanner:
        if entry.is_file():
            yield entry
        elif entry.is_dir():
            for entry in _scan_files(entry.path):
                yield entry


PreFilesIndex = Dict[str, int | None]


def _init_pre_files_index(run: Run) -> PreFilesIndex:
    return {path: modified for event, type, modified, path in _iter_files_log(run)}


class LoggedFile(NamedTuple):
    event: LoggedFileEvent
    type: RunFileType
    modified: int | None
    path: str


def _iter_files_log(run: Run):
    schema = _run_meta_schema(run)
    if schema != META_SCHEMA:
        raise TypeError(f"unsupported meta schema: {schema!r}")
    filename = _meta_files_log_filename(run)
    try:
        f = open(filename)
    except FileNotFoundError:
        pass
    else:
        lineno = 1
        for line in f:
            try:
                yield _decode_files_log_line(line.rstrip())
            except TypeError:
                raise TypeError(
                    f"bad encoding in \"{filename}\", line {lineno}: {line!r}"
                )
            lineno += 1


def _decode_files_log_line(line: str):
    parts = line.split(" ", 3)
    if len(parts) != 4:
        raise TypeError()
    event, type, modified_str, path = parts
    if modified_str == "-":
        modified = None
    else:
        try:
            modified = int(modified_str)
        except ValueError:
            raise TypeError()
    if event not in ("a", "d", "m"):
        raise TypeError()
    if type not in ("s", "d", "r", "g"):
        raise TypeError()
    return LoggedFile(event, type, modified, path)


def _open_files_log(run: Run):
    filename = _meta_files_log_filename(run)
    ensure_dir(os.path.dirname(filename))
    return open(filename, "a")


def _encode_logged_file(file: LoggedFile):
    return f"{file.event} {file.type} {file.modified or '-'} {file.path}\n"


# =================================================================
# Run manifest
# =================================================================


class RunManifestDecodeError(Exception):
    pass


class RunManifestEntry(NamedTuple):
    type: RunFileType
    digest: str
    path: str


class RunManifest:
    def __init__(self, run: Run, mode: Literal["r", "w", "a"] = "r"):
        self.filename = _meta_manifest_filename(run)
        try:
            self._f = open(self.filename, mode)
        except Exception as e:
            log.warning("Error reading manifest %s: %s", self.filename, e)
            self._f = io.StringIO()

    def __iter__(self):
        for line in self._f:
            yield _decode_run_manifest_entry(line)

    def close(self):
        self._f.close()

    def __enter__(self):
        self._f.__enter__()
        return self

    def __exit__(self, *exc: Any):
        self._f.__exit__(*exc)

    def add(self, type: RunFileType, digest: str, path: str):
        self._f.write(_encode_run_manifest_entry(type, digest, path))


def _encode_run_manifest_entry(type: RunFileType, digest: str, path: str):
    return f"{type} {digest} {path}\n"


def _decode_run_manifest_entry(entry: str) -> RunManifestEntry:
    parts = entry.rstrip().split(" ", 2)
    if len(parts) != 3:
        raise RunManifestDecodeError("invalid entry: {entry!r}")
    return RunManifestEntry(cast(RunFileType, parts[0]), parts[1], parts[2])


def _init_manifest_index(run: Run) -> dict[str, str]:
    index = {}
    with RunManifest(run) as m:
        for type, digest, path in m:
            index[path] = digest
    return index


# =================================================================
# Util
# =================================================================


def _write_patched(run: Run, diffs: list[tuple[str, UnifiedDiff]]):
    filename = _meta_patched_filename(run)
    with open(filename, "w") as f:
        for path, diff in sorted(diffs):
            for line in diff:
                f.write(line)
    set_readonly(filename)


class RunExecError(Exception):
    pass


class _PhaseExecOutputCallback(run_output.OutputCallback):
    def __init__(self, phase_name: str):
        self.phase_name = phase_name

    def output(self, stream: run_output.StreamType, out: bytes):
        run_phase_channel.notify("exec-output", (self.phase_name, stream, out))

    def close(self):
        pass


def _run_phase_exec(
    run: Run,
    phase_name: str,
    exec_cmd: str | list[str],
    env: dict[str, str],
    output_name: str,
    log: Logger,
):
    log.info(f"Running {phase_name} (see output/{output_name}): {exec_cmd}")
    proc_args, use_shell = _proc_args(exec_cmd)
    proc_env = {
        **os.environ,
        **env,
    }
    ensure_dir(run.run_dir)
    p = subprocess.Popen(
        proc_args,
        shell=use_shell,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        cwd=run.run_dir,
        env=proc_env,
    )
    ensure_dir(run_meta_path(run, "output"))
    output_filename = run_meta_path(run, "output", output_name)
    output_cb = _PhaseExecOutputCallback(phase_name)
    output = run_output.RunOutput(output_filename, output_cb=output_cb)
    output.open(p)
    exit_code = p.wait()
    output.wait_and_close()
    log.info(f"Exit code for {phase_name}: {exit_code}")
    set_readonly(output_filename)
    set_readonly(output_filename + ".index")
    if exit_code != 0:
        raise RunExecError(phase_name, proc_args, exit_code)


def _proc_args(exec_cmd: str | list[str]) -> tuple[str | list[str], bool]:
    if isinstance(exec_cmd, list):
        return exec_cmd, False
    line1, *rest = exec_cmd.splitlines()
    if line1.startswith("#!"):
        return [line1[2:].rstrip(), "-c", "".join(rest)], False
    else:
        return exec_cmd, True


def _hook_env(run: Run, project_dir: str | None = None):
    return {
        "run_id": run.id,
        "run_dir": run.run_dir,
        **({"project_dir": project_dir} if project_dir else {}),
    }


def format_run_timestamp(ts: datetime.datetime | None):
    if not ts:
        return ""
    return ts.strftime("%x %X")
