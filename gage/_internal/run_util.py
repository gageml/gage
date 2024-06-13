# SPDX-License-Identifier: Apache-2.0

from typing import *

from logging import Logger

from .types import *

import datetime
import io
import json
import logging
import os
import re
import subprocess
import threading
import time
import uuid
import zipfile

from proquint import uint2quint

from . import attr_log
from . import channel
from . import run_config_util
from . import run_meta
from . import run_sourcecode
from . import run_output
from . import shlex_util

from .file_select import copy_files

from .file_util import ensure_dir
from .file_util import file_sha256
from .file_util import is_readonly
from .file_util import ls
from .file_util import make_dir
from .file_util import safe_delete_tree
from .file_util import set_readonly

from .run_attr import run_project_ref
from .run_attr import run_user_dir

from .progress_util import progress_parser
from .project_util import load_project_data
from .sys_config import get_user

__all__ = [
    "META_SCHEMA",
    "OutputName",
    "RunExecError",
    "RunFileType",
    "RunManifest",
    "RunManifestEntry",
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
    "remove_associate_project",
    "run_for_meta_dir",
    "run_name_for_id",
    "run_phase_channel",
    "stage_dependencies",
    "stage_run",
    "stage_runtime",
    "stage_sourcecode",
    "exec_run",
]

META_SCHEMA = "1"

log = logging.getLogger(__name__)

run_phase_channel = channel.Channel()


# =================================================================
# Load run
# =================================================================


def run_for_meta_dir(meta_dir: str):
    try:
        opref = run_meta.read_opref(meta_dir)
    except (OSError, ValueError):
        return None
    else:
        run_dir = _run_dir_for_meta_dir(meta_dir)
        run_id = _run_id_for_meta_dir(meta_dir)
        run_name = run_name_for_id(run_id)
        return Run(run_id, opref, meta_dir, run_dir, run_name)


def _run_id_for_meta_dir(meta_dir: str):
    try:
        return _load_run_id(meta_dir)
    except (OSError, ValueError):
        dir_basename = os.path.basename(meta_dir)
        if meta_dir.endswith(".meta.deleted"):
            return dir_basename[:-13]
        elif meta_dir.endswith(".meta.zip"):
            return dir_basename[:-9]
        elif meta_dir.endswith(".meta.zip.deleted"):
            return dir_basename[:-17]
        else:
            assert dir_basename.endswith(".meta")
            return dir_basename[:-5]


def _load_run_id(meta_dir: str):
    filename = os.path.join(meta_dir, "id")
    with open(filename) as f:
        return f.read().rstrip()


def _run_dir_for_meta_dir(meta_dir: str):
    if meta_dir.endswith(".meta"):
        return meta_dir[:-5]
    elif meta_dir.endswith(".meta.deleted"):
        return meta_dir[:-13] + ".deleted"
    elif meta_dir.endswith(".meta.zip"):
        return meta_dir[:-9]
    elif meta_dir.endswith(".meta.zip.deleted"):
        return meta_dir[:-17] + ".deleted"
    else:
        assert False, meta_dir


# =================================================================
# Make run
# =================================================================


def make_run(opref: OpRef, location: str, id: str | None = None):
    run_id = id or make_run_id()
    run_dir = os.path.join(location, run_id)
    meta_dir = run_meta.make_meta_dir(run_dir)
    run_meta.write_opref(meta_dir, opref)
    run_name = run_name_for_id(run_id)
    return Run(run_id, opref, meta_dir, run_dir, run_name)


def make_run_id(_id_time: int = 0):
    if _id_time > 0:
        import ulid

        return str(ulid.ULID.from_timestamp(_id_time).to_uuid4())
    return str(uuid.uuid4())


def run_name_for_id(run_id: str) -> str:
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
    log = run_meta.runner_log(run)
    _write_run_id(run, log)
    _write_opdef(opdef, run, log)
    _write_config(config, run, log)
    _write_proc_cmd(cmd.args, run, log)
    _write_proc_env(cmd.env, run, log)
    if system_attrs:
        _write_system_attrs(system_attrs, run, log)
    _write_timestamp("initialized", run, log)


def _write_schema_file(run: Run):
    run_meta.write_schema(run, str(META_SCHEMA))


def _write_run_id(run: Run, log: Logger):
    log.info("Writing meta run id")
    run_meta.write_run_id(run)


def _write_opdef(opdef: OpDef, run: Run, log: Logger):
    log.info("Writing meta opdef")
    run_meta.write_opdef(run, opdef)


def _write_config(config: RunConfig, run: Run, log: Logger):
    log.info("Writing meta config")
    run_meta.write_config(run, config)


def _write_proc_cmd(args: CmdArgs, run: Run, log: Logger):
    log.info("Writing meta proc cmd")
    run_meta.write_proc_cmd(run, args)


def _write_proc_env(env: dict[str, str], run: Run, log: Logger):
    log.info("Writing meta proc env")
    run_meta.write_proc_env(run, env)


def _write_system_attrs(attrs: dict[str, Any], run: Run, log: Logger):
    for name in attrs:
        log.info("Writing meta system %s", name)
        run_meta.write_system_attribute(run, name, attrs[name])


# =================================================================
# Associate project
# =================================================================


def associate_project(run: Run, project_dir: str):
    if not os.path.isabs(project_dir):
        raise ValueError(f"project_dir must be absolute: \"{project_dir}\"")
    ref_filename = run_project_ref(run)
    project_ref_path = _project_ref_path(project_dir, run)
    with open(ref_filename, "w") as f:
        f.write(f"file:{project_ref_path}")


def _project_ref_path(project_dir: str, run: Run):
    run_parent = os.path.dirname(run.run_dir)
    assert os.path.isabs(project_dir) and os.path.isabs(run_parent)
    if run_parent.startswith(project_dir):
        return os.path.relpath(project_dir, run_parent)
    return project_dir


def remove_associate_project(run: Run):
    ref_filename = run_project_ref(run)
    try:
        os.remove(ref_filename)
    except FileNotFoundError:
        pass


# =================================================================
# Run user attrs
# =================================================================


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


class OutputName:
    sourcecode = "10_sourcecode"
    runtime = "20_runtime"
    dependencies = "30_dependencies"
    run = "40_run"


def stage_run(run: Run, project_dir: str):
    stage_sourcecode(run, project_dir, _log_files=False)
    apply_config(run)
    stage_runtime(run, project_dir)
    stage_dependencies(run, project_dir)
    finalize_staged_run(run)


def stage_sourcecode(run: Run, project_dir: str, _log_files: bool = True):
    log = run_meta.runner_log(run)
    opdef = run_meta.read_opdef(run)
    run_phase_channel.notify("stage-sourcecode", run)
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
    if not exec:
        return
    _run_phase_exec(
        run,
        "stage-sourcecode",
        exec,
        _hook_env(run, project_dir),
        opdef.get_progress().get_stage_sourcecode(),
        OutputName.sourcecode,
        log,
    )


def apply_config(run: Run):
    log = run_meta.runner_log(run)
    config = run_meta.read_config(run)
    opdef = run_meta.read_opdef(run)
    run_phase_channel.notify("stage-config", run)
    log.info("Applying configuration (see log/patched)")
    diffs = run_config_util.apply_config(config, opdef, run.run_dir)
    if diffs:
        run_meta.write_patched(run, diffs)
    _apply_to_files_log(run, "s")


def stage_runtime(run: Run, project_dir: str):
    log = run_meta.runner_log(run)
    opdef = run_meta.read_opdef(run)
    run_phase_channel.notify("stage-runtime", run)
    _stage_runtime_hook(run, project_dir, opdef, log)
    _apply_to_files_log(run, "r")


def _stage_runtime_hook(run: Run, project_dir: str, opdef: OpDef, log: Logger):
    exec = opdef.get_exec().get_stage_runtime()
    if not exec:
        return
    _run_phase_exec(
        run,
        "stage-runtime",
        exec,
        _hook_env(run, project_dir),
        opdef.get_progress().get_stage_runtime(),
        OutputName.runtime,
        log,
    )


def stage_dependencies(run: Run, project_dir: str):
    log = run_meta.runner_log(run)
    opdef = run_meta.read_opdef(run)
    run_phase_channel.notify("stage-dependencies", run)
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
    if not exec:
        return
    _run_phase_exec(
        run,
        "stage-dependencies",
        exec,
        _hook_env(run, project_dir),
        opdef.get_progress().get_stage_dependencies(),
        OutputName.dependencies,
        log,
    )


def finalize_staged_run(run: Run):
    log = run_meta.runner_log(run)
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
    paths: dict[str, RunFileType] = {}
    for event, type, modified, path in _iter_files_log(run):
        if event == "a":
            paths[path] = type
        elif event == "d":
            paths.pop(path, None)
    for path, type in paths.items():
        yield type, path


# =================================================================
# Exec run
# =================================================================


def exec_run(run: Run):
    log = run_meta.runner_log(run)
    opdef = run_meta.read_opdef(run)
    cmd = OpCmd(
        run_meta.read_proc_cmd(run),
        run_meta.read_proc_env(run),
    )
    env = {**_run_env(run), **cmd.env}
    run_phase_channel.notify("run", run)
    _write_timestamp("started", run, log)
    _run_phase_exec(
        run,
        "run",
        cmd.args,
        env,
        opdef.get_progress().get_run(),
        OutputName.run,
        log,
    )


def _run_env(run: Run):
    return {
        "RUN_ID": run.id,
        "RUN_DIR": run.run_dir,
        "PARENT_PWD": os.getcwd(),
        "PYTHONDONTWRITEBYTECODE": "1",
    }


# =================================================================
# Finalize run
# =================================================================


def finalize_run(run: Run, exit_code: int = 0):
    log = run_meta.runner_log(run)
    opdef = run_meta.read_opdef(run)
    run_phase_channel.notify("finalize", run)
    ensure_dir(run.run_dir)
    _finalize_run_summary(run, opdef, log)
    _write_timestamp("stopped", run, log)
    _write_exit_code(exit_code, run, log)
    _finalize_run_hook(run, opdef, log)
    _apply_to_files_log(run, "g")
    _write_run_files_manifest(run, log)
    if os.getenv("NO_ZIP_META") != "1":
        zip_filename = _zip_meta(run)
        return run_for_meta_dir(zip_filename)
    else:
        return run


def _finalize_run_summary(run: Run, opdef: OpDef, log: Logger):
    summary = _load_run_summary(run, opdef, log)
    _apply_opdef_summary(opdef, summary)
    _write_meta_summary(summary, run, log)


def _load_run_summary(run: Run, opdef: OpDef, log: Logger) -> RunSummary:
    return (
        _run_summary_from_file(run, opdef, log)
        or _run_summary_from_output(run, opdef, log)
        or RunSummary({})
    )


def _run_summary_from_file(run: Run, opdef: OpDef, log: Logger):
    filename = _run_summary_filename(run, opdef)
    if not os.path.exists(filename):
        return None
    log.info(f"Using run summary '{os.path.relpath(filename, run.run_dir)}'")
    return RunSummary(load_project_data(filename))


def _run_summary_filename(run: Run, opdef: OpDef):
    name = opdef.get_summary().get_filename() or "summary.json"
    return os.path.join(run.run_dir, name)


_DEFAULT_OUTPUT_SUMMARY_PATTERN = "--- summary ---(.*)---"


def _run_summary_from_output(run: Run, opdef: OpDef, log: Logger):
    summary_pattern = opdef.get_output_summary_pattern()
    if summary_pattern is False or summary_pattern == "":
        return None
    if summary_pattern is True:
        summary_pattern = None
    try:
        output = run_meta.read_output(run, OutputName.run)
    except OSError as e:
        log.info("Error reading ${OutputName.run} output for summary: {e}")
        return None
    else:
        if summary_pattern:
            return _try_summary_pattern(output, summary_pattern, log)
        return (
            _try_decode_summary(output)
            or _try_summary_pattern(output, _DEFAULT_OUTPUT_SUMMARY_PATTERN, log)
            # \
        )


def _try_summary_pattern(out: str, pattern: str, log: Logger):
    log.info(f"Checking output for summary pattern {pattern!r}")
    try:
        m = re.search(pattern, out, re.DOTALL)
    except re.error as e:
        log.info(f"Error in output summary pattern {pattern!r}: {e}")
        return None
    else:
        if not m:
            return None
        log.info("Found summary output")
        try:
            encoded = m.group(1)
        except IndexError:
            log.info(
                f"Error in output summary pattern {pattern!r}: must capture a group"
            )
            return None
        else:
            return _try_decode_summary(encoded, log)


def _try_decode_summary(out: str, log: Logger | None = None) -> RunSummary | None:
    try:
        data = json.loads(out)
    except json.JSONDecodeError as e:
        if log:
            log.info(f"Error decoding output summary: {e}")
        return None
    else:
        if not _is_summary_data(data):
            if log:
                log.info(
                    "Decoded output is not valid summary data: missing "
                    "metrics or attributes"
                )
            return None
        return RunSummary(data)


def _is_summary_data(data: Any):
    return isinstance(data, dict) and ("metrics" in data or "attributes" in data)


def _apply_opdef_summary(opdef: OpDef, summary: RunSummary):
    # TODO - merge content from opdef summary to summary
    pass


def _write_meta_summary(summary: RunSummary, run: Run, log: Logger):
    log.info("Writing meta summary")
    run_meta.write_summary(run, summary)


def _write_exit_code(exit_code: int, run: Run, log: Logger):
    log.info("Writing meta proc/exit")
    run_meta.write_proc_exit(run, exit_code)


def _finalize_run_hook(run: Run, opdef: OpDef, log: Logger):
    exec = opdef.get_exec().get_finalize()
    if not exec:
        return
    _run_phase_exec(
        run,
        "finalize",
        exec,
        _hook_env(run),
        opdef.get_progress().get_finalize(),
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


def _zip_meta(run: Run):
    filename = _make_meta_zip(run)
    safe_delete_tree(run.meta_dir)
    return filename


def _make_meta_zip(run: Run):
    files = ls(run.meta_dir, followlinks=True, include_dirs=True)
    filename = _meta_zip_filename(run)
    with zipfile.ZipFile(filename, "x") as zf:
        for path in files:
            zf.write(os.path.join(run.meta_dir, path), path)
    return filename


def _meta_zip_filename(run: Run):
    return run.meta_dir + ".zip"


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


def _write_timestamp(name: RunTimestamp, run: Run, log: Logger):
    log.info(f"Writing meta {name}")
    with run_meta.open_meta_file(run, name, write=True) as f:
        f.write(str(make_run_timestamp()))


def _apply_to_files_log(run: Run, type: RunFileType):
    pre_files = _init_pre_files_index(run)
    seen = set()
    with run_meta.open_files_log(run, append=True) as f:
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
    schema = run_meta.read_schema(run)
    if schema != META_SCHEMA:
        raise TypeError(f"unsupported meta schema: {schema!r}")
    try:
        f = run_meta.open_files_log(run)
    except FileNotFoundError:
        pass
    else:
        lineno = 1
        for line in f:
            try:
                yield _decode_files_log_line(line.rstrip())
            except TypeError:
                raise TypeError(
                    f"bad encoding in \"{f.name}\", line {lineno}: {line!r}"
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
        try:
            self._f = run_meta.open_manifest(run, write=mode == "w", append=mode == "a")
        except Exception as e:
            log.warning("Error reading manifest in %s: %s", run.meta_dir, e)
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


class RunExecError(Exception):
    def __init__(self, phase_name: str, proc_args: str | list[str], exit_code: int):
        self.phase_name = phase_name
        self.proc_args = proc_args
        self.exit_code = exit_code


class _PhaseExecOutputCallback(run_output.OutputCallback):
    def __init__(self, run: Run, phase_name: str):
        self.run = run
        self.phase_name = phase_name

    def output(
        self,
        stream: run_output.StreamType,
        out: bytes,
        progress: Any | None = None,
    ):
        run_phase_channel.notify(
            "exec-output", (self.run, self.phase_name, stream, out, progress)
        )

    def close(self):
        pass


def _progress_parser(progress: str | None):
    return progress_parser(progress) if progress else None


def _run_phase_exec(
    run: Run,
    phase_name: str,
    exec_cmd: str | list[str],
    env: dict[str, str],
    progress: str | None,
    output_name: str,
    log: Logger,
):
    log.info(f"Starting {phase_name} (see output/{output_name}): {exec_cmd}")
    proc_args, cmd_env, use_shell = _proc_args(exec_cmd)
    proc_env = {
        **env,
        **os.environ,
        **cmd_env,
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
    _write_proc_lock(p, run, log)
    output_cb = _PhaseExecOutputCallback(run, phase_name)
    progress_parser = _progress_parser(progress)
    output = run_meta.run_output_writer(
        run,
        output_name,
        output_cb=output_cb,
        progress_parser=progress_parser,
    )
    output.open(p)
    exit_code = p.wait()
    output.wait_and_close()
    log.info(f"Exit code for {phase_name}: {exit_code}")
    _delete_proc_lock(run, log)
    if exit_code != 0:
        raise RunExecError(phase_name, proc_args, exit_code)


def _proc_args(
    exec_cmd: str | list[str],
) -> tuple[str | list[str], dict[str, str], bool]:
    if isinstance(exec_cmd, list):
        return exec_cmd, {}, False
    line1, *rest = exec_cmd.splitlines()
    if line1.startswith("#!"):
        return [line1[2:].rstrip(), "-c", "".join(rest)], {}, False
    elif os.name == "nt":
        cmd, env = shlex_util.split_env(exec_cmd)
        return cmd, env, True
    else:
        return exec_cmd, {}, True


def _hook_env(run: Run, project_dir: str | None = None):
    return {
        "run_id": run.id,
        "run_dir": run.run_dir,
        **({"project_dir": project_dir} if project_dir else {}),
    }


def _write_proc_lock(proc: subprocess.Popen[bytes], run: Run, log: Logger):
    log.info("Writing meta proc/lock")
    run_meta.write_proc_lock(run, proc.pid)


def _delete_proc_lock(run: Run, log: Logger):
    log.info("Deleting meta proc/lock")
    run_meta.delete_proc_lock(run)


def format_run_timestamp(ts: datetime.datetime | None):
    if not ts:
        return ""
    return ts.strftime("%x %X")
