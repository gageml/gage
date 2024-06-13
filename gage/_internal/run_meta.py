# SPDX-License-Identifier: Apache-2.0

from typing import *

import io
import json
import logging
import os
import zipfile

from .types import *

from .file_util import ensure_dir
from .file_util import make_dir

from .opref_util import encode_opref
from .opref_util import decode_opref

log = logging.getLogger(__name__)

# =================================================================
# File utils
# =================================================================


def make_meta_dir(run_dir: str):
    meta_dir = run_dir + ".meta"
    make_dir(meta_dir)
    return meta_dir


def meta_file_exists(run: Run, *path: str):
    if _is_zip(run.meta_dir):
        return _meta_zip_file_exists(run.meta_dir, _zip_member(path))
    else:
        return os.path.exists(os.path.join(run.meta_dir, *path))


def open_meta_file(run: Run, *path: str, write: bool = False, append: bool = False):
    if write and append:
        raise ValueError("cannot specify both write and append")
    return _open_meta_file(run.meta_dir, path, write, append)


def _open_meta_file(
    meta_dir: str,
    path: list[str] | tuple[str, ...],
    write: bool = False,
    append: bool = False,
):
    if _is_zip(meta_dir):
        if write or append:
            raise ValueError(f"cannot write to {meta_dir}")
        return _open_meta_zip_file(meta_dir, _zip_member(path))
    else:
        return _open_meta_dir_file(meta_dir, path, write, append)


def _zip_member(path: list[str] | tuple[str, ...]):
    return "/".join(path)


def _is_zip(meta_dir: str):
    return meta_dir.endswith(".zip") or meta_dir.endswith(".zip.deleted")


def _open_meta_dir_file(
    meta_dir: str,
    path: list[str] | tuple[str, ...],
    write: bool = False,
    append: bool = False,
):
    assert not (write and append)
    filename = os.path.join(meta_dir, *path)
    if write or append:
        ensure_dir(os.path.dirname(filename))
    mode = "w" if write else "a" if append else "r"
    return open(filename, mode)


@overload
def _open_meta_zip_file(
    filename: str, member: str, text: Literal[True] = True
) -> IO[str]: ...


@overload
def _open_meta_zip_file(
    filename: str, member: str, text: Literal[False]
) -> IO[bytes]: ...


def _open_meta_zip_file(filename: str, member: str, text: bool = True):
    zf = zipfile.ZipFile(filename)
    f = zf.open(member)
    f.close = _zip_member_close_f(f, zf)
    return cast(IO[str], io.TextIOWrapper(f)) if text else f


def _zip_member_close_f(f: IO[Any], zf: zipfile.ZipFile):
    f_close = f.close

    def close():
        f_close()
        zf.close()

    return close


def _meta_zip_file_exists(filename: str, member: str):
    with zipfile.ZipFile(filename) as zf:
        try:
            zf.getinfo(member)
        except KeyError:
            return False
        else:
            return True


# =================================================================
# Schema
# =================================================================


def read_schema(run: Run):
    with _open_schema(run) as f:
        return f.read().rstrip()


def _open_schema(run: Run, write: bool = False):
    return _open_meta_file(run.meta_dir, ["__schema__"], write)


def write_schema(run: Run, schema: str):
    with _open_schema(run, write=True) as f:
        f.write(str(schema))


# =================================================================
# Opref
# =================================================================


def read_opref(run_or_meta_dir: Run | str) -> OpRef:
    with _open_opref(run_or_meta_dir) as f:
        return decode_opref(f.read())


def _open_opref(run_or_meta_dir: Run | str, write: bool = False):
    meta_dir = (
        run_or_meta_dir
        if isinstance(run_or_meta_dir, str)
        else run_or_meta_dir.meta_dir
    )
    return _open_meta_file(meta_dir, ["opref"], write)


def write_opref(meta_dir: str, opref: OpRef):
    with _open_opref(meta_dir, write=True) as f:
        f.write(encode_opref(opref))


# =================================================================
# Run ID
# =================================================================


def write_run_id(run: Run):
    with _open_run_id(run, write=True) as f:
        f.write(run.id)


def _open_run_id(run: Run, write: bool = False):
    return _open_meta_file(run.meta_dir, ["id"], write)


# =================================================================
# Opdef
# =================================================================


def read_opdef(run: Run) -> OpDef:
    opref = read_opref(run)
    with _open_opdef(run) as f:
        opdef_data = json.load(f)
    return OpDef(opref.op_name, opdef_data)


def _open_opdef(run: Run, write: bool = False):
    return _open_meta_file(run.meta_dir, ["opdef.json"], write)


def write_opdef(run: Run, opdef: OpDef):
    with _open_opdef(run, write=True) as f:
        json.dump(opdef.as_json(), f, indent=2, sort_keys=True)


# =================================================================
# Config
# =================================================================


def read_config(run: Run) -> RunConfig:
    with _open_config(run) as f:
        return json.load(f)


def _open_config(run: Run, write: bool = False):
    return _open_meta_file(run.meta_dir, ["config.json"], write)


def write_config(run: Run, config: RunConfig):
    with _open_config(run, write=True) as f:
        json.dump(config, f, indent=2, sort_keys=True)


# =================================================================
# Summary
# =================================================================


def read_summary(run: Run):
    with _open_summary(run) as f:
        return json.load(f)


def _open_summary(run: Run, write: bool = False):
    return _open_meta_file(run.meta_dir, ["summary.json"], write)


def write_summary(run: Run, summary: RunSummary):
    with _open_summary(run, write=True) as f:
        json.dump(summary.as_json(), f, indent=2, sort_keys=True)


# =================================================================
# Proc cmd
# =================================================================


def read_proc_cmd(run: Run) -> CmdArgs:
    with _open_proc_cmd(run) as f:
        return json.load(f)


def _open_proc_cmd(run: Run, write: bool = False):
    return _open_meta_file(run.meta_dir, ["proc", "cmd.json"], write)


def write_proc_cmd(run: Run, cmd: CmdArgs):
    with _open_proc_cmd(run, write=True) as f:
        json.dump(cmd, f)


# =================================================================
# Proc env
# =================================================================


def read_proc_env(run: Run) -> dict[str, str]:
    with _open_proc_env(run) as f:
        return json.load(f)


def _open_proc_env(run: Run, write: bool = False):
    return _open_meta_file(run.meta_dir, ["proc", "env.json"], write)


def write_proc_env(run: Run, env: dict[str, str]):
    with _open_proc_env(run, write=True) as f:
        json.dump(env, f, indent=2, sort_keys=True)


# =================================================================
# Proc exit
# =================================================================


def read_proc_exit(run: Run):
    with _open_proc_exit(run) as f:
        return f.read().rstrip()


def _open_proc_exit(run: Run, write: bool = False):
    return _open_meta_file(run.meta_dir, ["proc", "exit"], write)


def write_proc_exit(run: Run, proc_exit: int):
    with _open_proc_exit(run, write=True) as f:
        f.write(str(proc_exit))


# =================================================================
# Proc lock
# =================================================================


def read_proc_lock(run: Run):
    with _open_proc_lock(run) as f:
        return f.read().rstrip()


def _open_proc_lock(run: Run, write: bool = False):
    # proc/lock is only applicable to meta dirs
    return _open_meta_dir_file(run.meta_dir, ["proc", "lock"], write)


def write_proc_lock(run: Run, pid: int):
    with _open_proc_lock(run, write=True) as f:
        f.write(str(pid))


def delete_proc_lock(run: Run):
    if _is_zip(run.meta_dir):
        raise ValueError("cannot delete from zip")
    filename = os.path.join(run.meta_dir, "proc", "lock")
    try:
        os.remove(filename)
    except OSError as e:
        log.info(f"Error deleting proc/lock: {e}")


# =================================================================
# System attributes
# =================================================================


def write_system_attribute(run: Run, name: str, value: Any):
    with _open_system_attribute(run, name, write=True) as f:
        json.dump(value, f, indent=2, sort_keys=True)


def _open_system_attribute(run: Run, name: str, write: bool = False):
    return _open_meta_file(run.meta_dir, ["sys", name + ".json"], write)


# =================================================================
# Run output
# =================================================================


def read_output(run: Run, output_name: str):
    with _open_run_output(run, output_name) as f:
        return f.read()


def _open_run_output(run: Run, output_name: str, write: bool = False):
    return _open_meta_file(run.meta_dir, ["output", output_name], write)


def writeable_output_filename(run: Run, output_name: str):
    if _is_zip(run.meta_dir):
        raise ValueError(f"cannot write to {run.meta_dir}")
    output_dir = os.path.join(run.meta_dir, "output")
    ensure_dir(output_dir)
    return os.path.join(output_dir, output_name)


# =================================================================
# Runner log
# =================================================================


def runner_log(run: Run):
    filename = _log_filename(run.meta_dir, "runner")
    ensure_dir(os.path.dirname(filename))
    runner_log = logging.Logger("runner")
    handler = logging.FileHandler(filename)
    runner_log.addHandler(handler)
    if log.getEffectiveLevel() <= logging.INFO:
        runner_log.addHandler(logging.StreamHandler())
    formatter = logging.Formatter("%(asctime)s %(message)s", "%Y-%m-%dT%H:%M:%S%z")
    handler.setFormatter(formatter)
    return runner_log


def _log_filename(meta_dir: str, log_name: str):
    if _is_zip(meta_dir):
        raise ValueError(f"cannot write to {meta_dir}")
    return os.path.join(meta_dir, "log", log_name)


# =================================================================
# Files log
# =================================================================


def open_files_log(run: Run, append: bool = False):
    return _open_meta_file(run.meta_dir, ["log", "files"], append=append)


# =================================================================
# Manifest
# =================================================================


def open_manifest(run: Run, write: bool = False, append: bool = False):
    return _open_meta_file(run.meta_dir, ["manifest"], write=write, append=append)


# =================================================================
# Patched
# =================================================================


def write_patched(run: Run, diffs: list[tuple[str, UnifiedDiff]]):
    with _open_patched(run, write=True) as f:
        for path, diff in sorted(diffs):
            for line in diff:
                f.write(line)


def _open_patched(run: Run, write: bool = True):
    return _open_meta_file(run.meta_dir, ["log", "patched"], write)
