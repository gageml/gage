# SPDX-License-Identifier: Apache-2.0

from typing import *

import datetime
import difflib
import fnmatch
import errno
import json
import os
import pprint
import re
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time

import gage

from . import sys_config
from . import file_util
from . import shlex_util
from . import util

__all__ = [
    "Env",
    "LogCapture",
    "StderrCapture",
    "SysPath",
    "basename",
    "cat",
    "cat_json",
    "cat_log",
    "cd",
    "compare_paths",
    "copytree",
    "datetime_now",
    "datetime_fromiso",
    "delete_temp_dir",
    "diff",
    "diffl",
    "lsl",
    "json",
    "json_pprint",
    "ls",
    "make_dir",
    "make_temp_dir",
    "normlf",
    "os",
    "parse_any",
    "parse_comment_id",
    "parse_datetime",
    "parse_isodate",
    "parse_path",
    "parse_run_id",
    "parse_run_name",
    "parse_short_run_id",
    "parse_short_run_name",
    "parse_sha256",
    "parse_timestamp",
    "parse_timestamp_ms",
    "parse_timestamp_s",
    "parse_uuid4",
    "parse_ver",
    "path_exists",
    "path_join",
    "pprint",
    "printl",
    "quiet",
    "re",
    "rm",
    "run",
    "sample",
    "samples_dir",
    "set_runs_home",
    "sha256",
    "sleep",
    "symlink",
    "sys",
    "touch",
    "udiff",
    "use_example",
    "use_project",
    "write",
]

# Pass-through

Env = util.Env
LogCapture = util.LogCapture
StderrCapture = util.StderrCapture
basename = os.path.basename
delete_temp_dir = file_util.delete_temp_dir
compare_paths = file_util.compare_paths
copytree = file_util.copy_tree
lsl = file_util.ls
pprint = pprint.pprint
sha256 = file_util.file_sha256
sleep = time.sleep
symlink = os.symlink
touch = file_util.touch


def parse_type(name: str, pattern: str, group_count: int = 0):
    def decorator(f: Callable[[str], Any]):
        f.type_name = name
        f.pattern = pattern
        f.regex_group_count = group_count
        return f

    return decorator


@parse_type("any", r"[^\n]+")
def parse_any(s: str):
    return s


# Simplified https://regex101.com/r/Ly7O1x/3/
VER_PATTERN = (
    r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-(?:0|[1-9]\d*|\d*[a-zA-Z-][a-zA-Z-]*\.[0-9]+))?"
)

__ver_pattern_compiled = re.compile(VER_PATTERN)


@parse_type("ver", VER_PATTERN, 3)
def parse_ver(s: str):
    m = __ver_pattern_compiled.match(s)
    if not m:
        return None
    return m.groups()


@parse_type("path", r"[/~][/\w_\-.:]*")
def parse_path(s: str):
    return s


@parse_type("run_id", r"[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}")
def parse_run_id(s: str):
    return s


@parse_type("uuid4", r"[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}")
def parse_uuid4(s: str):
    return s


@parse_type("rid", r"[a-f0-9]{8}")
def parse_short_run_id(s: str):
    return s


@parse_type("rn", r"[a-z]{5}")
def parse_short_run_name(s: str):
    return s


@parse_type("run_name", r"[a-z]{5}-[a-z]{5}")
def parse_run_name(s: str):
    return s


@parse_type("comment_id", r"[0-9a-f]{8}-[0-9a-f]{4}")
def parse_comment_id(s: str):
    return s


@parse_type("timestamp", r"1[6-7]\d{14}")  # epoch microseconds
def parse_timestamp(s: str):
    return int(s)


@parse_type("timestamp_ms", r"1[6-7]\d{11}")  # epoch seconds
def parse_timestamp_ms(s: str):
    return int(s)


@parse_type("timestamp_s", r"1[6-7]\d{8}")  # epoch seconds
def parse_timestamp_s(s: str):
    return int(s)


@parse_type("datetime", r"[\d\-/]+ [\d:]+")
def parse_datetime(s: str):
    return datetime.datetime.strptime(s, "%x %X").isoformat()


@parse_type("isodate", r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:[+-]\d{4}(?:\d{2})?)?")
def parse_isodate(s: str):
    return datetime.datetime.fromisoformat(_format_tz(s)).isoformat()


@parse_type("sha256", "[a-f0-9]{64}")
def parse_sha256(s: str):
    return s


def _format_tz(s: str):
    # Add ':' to tz component for parsing by `fromisoformat`
    tz = s[19:]
    if not tz:
        return s
    if len(tz) == 5:
        return s[:19] + tz[:3] + ":" + tz[3:]
    else:
        assert len(tz) == 7, s
        return s[:19] + tz[:3] + ":" + tz[3:5] + ":" + tz[5:]


def tests_dir():
    return os.path.join(gage.__pkgdir__, "tests")


def sample(*parts: str):
    return os.path.join(*(samples_dir(),) + parts)


def samples_dir():
    return os.path.join(tests_dir(), "samples")


def make_dir(dir: str):
    os.mkdir(dir)


def make_temp_dir(prefix: str = "gage-test-"):
    return tempfile.mkdtemp(prefix=prefix)


FindIgnore = str | list[str]


def ls(
    root: str = ".",
    follow_links: bool = False,
    include_dirs: bool = False,
    ignore: FindIgnore | None = None,
    permissions: bool = False,
):
    import natsort

    paths = file_util.ls(root, follow_links, include_dirs)
    if ignore:
        paths = _filter_ignored(paths, ignore)
    paths = _standardize_paths(paths)
    paths.sort(key=natsort.natsort_key)
    if not paths:
        print("<empty>")
    else:
        for path in paths:
            if permissions:
                perm = _ls_path_permissions(os.path.join(root, path))
                print(f"{perm} {path}")
            else:
                print(path)


def _ls_path_permissions(path: str):
    """Returns path permissions using `stat.filemode`.

    Group write permissions are coerced to be what the user write
    permission is. This is special case for tests, which may run in
    environments where the user group doesn't have write access by
    default. To handle these cases, the user write flag is used, if set,
    for the group write permission, even if the group does not have
    write permission.
    """
    perm = stat.filemode(os.stat(path).st_mode)
    assert len(perm) == 10, (perm, path)
    if perm[2] == "w" and perm[5] == "-":
        return perm[:5] + "w" + perm[6:]
    return perm


def _filter_ignored(paths: list[str], ignore: FindIgnore):
    if isinstance(ignore, str):
        ignore = [ignore]
    return [
        p for p in paths if not any((fnmatch.fnmatch(p, pattern) for pattern in ignore))
    ]


def _standardize_paths(paths: Iterable[str]):
    return [file_util.standardize_path(path) for path in paths]


def cat(*parts: str):
    with open(os.path.join(*parts), "r") as f:
        s = f.read()
        if not s:
            print("<empty>")
        else:
            if s[-1:] == "\n":
                s = s[:-1]
            print(s)


def cat_json(filename: str):
    val = json.load(open(filename))
    print(json.dumps(val, indent=2, sort_keys=True))


def write(filename: str, contents: str, append: bool = False, force: bool = True):
    if force and os.path.exists(filename):
        file_util.set_readonly(filename, False)
    file_util.write_file(filename, contents, append=append)


class SysPath:
    _sys_path0 = None

    def __init__(
        self,
        path: Optional[list[str]] = None,
        prepend: Optional[list[str]] = None,
        append: Optional[list[str]] = None,
    ):
        path = path if path is not None else sys.path
        if prepend:
            path = prepend + path
        if append:
            path = path + append
        self.sys_path = path

    def __enter__(self):
        self._sys_path0 = sys.path
        sys.path = self.sys_path

    def __exit__(self, *exc: Any):
        assert self._sys_path0 is not None
        sys.path = self._sys_path0


def normlf(s: str):
    return s.replace("\r", "")


_Env = dict[str, str]


def quiet(cmd: str, **kw: Any):
    run(cmd, quiet=True, **kw)


def run(
    cmd: str,
    quiet: bool = False,
    ignore: str | list[str] | None = None,
    timeout: int = 3600,
    cwd: Optional[str] = None,
    env: Optional[_Env] = None,
    rstrip: bool = True,
    cols: int | None = None,
    _capture: bool = False,
    **other_env: str,
):
    proc_env = dict(os.environ)
    _apply_venv_bin_path(proc_env)
    proc_env["TERM"] = "unknown"
    if cols is None:
        proc_env["COLUMNS"] = "60"
    elif cols >= 0:
        proc_env["COLUMNS"] = str(cols)
    proc_env.update(env or {})
    proc_env.update(other_env)
    p = _popen(cmd, proc_env, cwd)
    with _kill_after(p, timeout) as timeout_context:
        try:
            out, err = p.communicate()
        except KeyboardInterrupt:
            # Handler for Ctrl-c - ideally would be an SIGINT handler
            # (see #485)
            timeout_context.kill_now()
            raise
        else:
            assert err is None, err
            exit_code = cast(int, p.returncode)
    if quiet and exit_code == 0:
        return None
    out = out.strip().decode("latin-1")
    if rstrip:
        out = _rstrip_lines(out)
    if ignore:
        out = _strip_lines(out, ignore)
    if _capture:
        return exit_code, out
    if out:
        print(out)
    print(f"<{exit_code}>")
    return None


def _apply_venv_bin_path(env: dict[str, str]):
    python_bin_dir = os.path.dirname(sys.executable)
    path = env.get("PATH") or ""
    if python_bin_dir not in path:
        env["PATH"] = f"{python_bin_dir}{os.path.pathsep}{path}"


def _popen(cmd: str, env: _Env, cwd: Optional[str]):
    if os.name == "nt":
        return _popen_win(cmd, env, cwd)
    return _popen_posix(cmd, env, cwd)


def _popen_win(cmd: str, env: _Env, cwd: Optional[str]):
    split_cmd = shlex_util.shlex_split(file_util.standardize_path(cmd))
    return subprocess.Popen(
        split_cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,  # type: ignore Windows only
        env=env,
        cwd=cwd,
    )


def _popen_posix(cmd: str, env: _Env, cwd: Optional[str]):
    cmd = f"set -eu && {cmd}"
    return subprocess.Popen(
        cmd,
        shell=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        preexec_fn=os.setsid,
        env=env,
        cwd=cwd,
    )


class _kill_after:
    def __init__(self, p: "subprocess.Popen[bytes]", timeout: int):
        self._p = p
        self._timer = threading.Timer(timeout, self._kill)

    def kill_now(self):
        self._kill()

    def _kill(self):
        if os.name == "nt":
            self._kill_win()
        else:
            self._kill_posix()

    def _kill_win(self):
        try:
            self._p.send_signal(signal.CTRL_BREAK_EVENT)  # type: ignore Windows only
            self._p.kill()
        except OSError as e:
            if e.errno != errno.ESRCH:  # no such process
                raise

    def _kill_posix(self):
        try:
            os.killpg(os.getpgid(self._p.pid), signal.SIGKILL)
        except OSError as e:
            if e.errno != errno.ESRCH:  # no such process
                raise

    def __enter__(self):
        self._timer.start()
        return self

    def __exit__(self, *exc: Any):
        self._timer.cancel()


def _rstrip_lines(out: str):
    return "\n".join([line.rstrip() for line in out.split("\n")])


def _strip_lines(out: str, patterns: str | list[str]):
    if isinstance(patterns, str):
        patterns = [patterns]
    stripped_lines = [
        line
        for line in out.split("\n")
        if not any((re.search(p, line) for p in patterns))
    ]
    return "\n".join(stripped_lines)


def cd(s: str):
    os.chdir(os.path.expandvars(s))


def set_runs_home(path: str):
    sys_config.set_runs_home(path)


def use_example(name: str, var_home: Optional[str] = None):
    var_home = var_home or make_temp_dir()
    cd(_example(name))
    set_runs_home(var_home)


def _example(name: str):
    return os.path.join(_examples_dir(), name)


def _examples_dir():
    try:
        return os.environ["EXAMPLES"]
    except KeyError:
        return os.path.join(gage.__pkgdir__, "examples")


def use_project(project_name: str, var_home: Optional[str] = None):
    var_home = var_home or make_temp_dir()
    cd(sample("projects", project_name))
    set_runs_home(var_home)


def path_join(*path: str):
    return os.path.join(*path)


def path_exists(path: str):
    return os.path.exists(path)


def printl(l: list[Any]):
    for x in l:
        print(x)


def udiff(s1: str, s2: str, n: int = 3):
    s1_lines = s1.splitlines(True)
    s2_lines = s2.splitlines(True)
    print("".join(difflib.unified_diff(s1_lines, s2_lines, n=n))[:-1])


def cat_log(filename: str):
    date_p = re.compile(parse_isodate.pattern)
    for line in open(filename).readlines():
        date, rest = line.split(" ", 1)
        assert date_p.match(date)
        print(rest, end="")


def datetime_now():
    now_local = time.localtime()
    return datetime.datetime.fromtimestamp(
        time.mktime(now_local),
        datetime.timezone(datetime.timedelta(seconds=now_local.tm_gmtoff)),
    )


def datetime_fromiso(s: str):
    return datetime.datetime.fromisoformat(s)


def json_pprint(data: Any):
    print(json.dumps(data, indent=2, sort_keys=True))


def diff(path1: str, path2: str):
    lines1 = [s.rstrip() for s in open(path1).readlines()]
    lines2 = [s.rstrip() for s in open(path2).readlines()]
    diff_lines = list(difflib.unified_diff(lines1, lines2, path1, path2, lineterm=""))
    for line in diff_lines[2:]:
        print(line)


def diffl(l1: list[str], l2: list[str]):
    diff_lines = list(difflib.unified_diff(l1, l2, "list 1", "list 2", lineterm=""))
    for line in diff_lines[2:]:
        print(line)


def rm(filename: str, force: bool = False):
    if force and not os.path.exists(filename):
        return
    os.remove(filename)
