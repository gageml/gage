# SPDX-License-Identifier: Apache-2.0

from typing import *

import datetime
import errno
import logging
import os
import re
import subprocess
import sys
import threading
import time

from . import ansi_util
from . import shlex_util
from . import log as loglib

# Avoid expensive imports.

log = logging.getLogger()

UNSAFE_OS_ENVIRON = set(["_"])

REF_P = re.compile(r"(\\?\${.+?})")


class Stop(Exception):
    """Raise to stop loops started with `loop`."""


class TryFailed(RuntimeError):
    """Raise to indicate an attempt in try_apply failed."""


T = TypeVar("T")
D = TypeVar("D")


def find_apply(
    funs: list[Callable[..., T]],
    *args: Any,
    default: D | None = None,
) -> T | D | None:
    for f in funs:
        result = f(*args)
        if result is not None:
            return result
    return default


def apply_acc(funs: list[Callable[..., T]], *args: Any) -> list[T]:
    return [x for x in [f(*args) for f in funs] if x is not None]


def try_apply(funs: Iterable[Callable[..., Any]], *args: Any):
    for f in funs:
        try:
            return f(*args)
        except TryFailed:
            continue
    raise TryFailed(funs, args)


def any_apply(funs: Iterable[Callable[..., Any]], *args: Any):
    for f in funs:
        if f(*args):
            return True
    return False


def all_apply(funs: list[Callable[..., Any]], *args: Any):
    for f in funs:
        if not f(*args):
            return False
    return True


def pop_find(l: list[Any], f: Callable[[Any], Any], default: Any = None):
    popped = default
    for x in list(l):
        if f(x):
            popped = x
            l.remove(x)
            break
    return popped


def ensure_deleted(path: str):
    try:
        os.remove(path)
    except OSError as e:
        if e.errno != errno.ENOENT:
            raise


_ReadApp = Callable[[str], Any]


def try_read(
    path: str,
    default: Any = None,
    apply: _ReadApp | list[_ReadApp] | None = None,
):
    try:
        f = open(path, "r")
    except IOError as e:
        if e.errno != errno.ENOENT:
            raise
        return default
    else:
        out = f.read()
        if apply:
            if not isinstance(apply, list):
                apply = [apply]
            for f in apply:
                out = f(out)
        return out


def pid_exists(pid: int, default: Optional[bool] = True):
    return find_apply(
        [
            _proc_pid_exists,
            _psutil_pid_exists,
            lambda _: default,
        ],
        pid,
    )


def _proc_pid_exists(pid: int):
    if os.path.exists("/proc"):
        return os.path.exists(os.path.join("/proc", str(pid)))
    return None


def _psutil_pid_exists(pid: int):
    try:
        import psutil
    except Exception as e:
        log.warning("cannot get status for pid %s: %s", pid, e)
        if log.getEffectiveLevel() <= logging.DEBUG:
            log.exception("importing psutil")
        return None
    return psutil.pid_exists(pid)


def free_port(start: Optional[int] = None) -> int:
    import random
    import socket

    min_port = 49152
    max_port = 65535
    max_attempts = 100
    attempts = 0
    if start is None:
        next_port = lambda _p: random.randint(min_port, max_port)
        port = next_port(None)
    else:
        next_port = lambda p: p + 1
        port = start
    while attempts <= max_attempts:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(0.1)
        try:
            sock.connect(("localhost", port))
        except socket.timeout:
            return port
        except socket.error as e:
            if e.errno == errno.ECONNREFUSED:
                return port
        else:
            sock.close()
        attempts += 1
        port = next_port(port)
    raise RuntimeError("too many free port attempts")


def open_url(url: str):
    try:
        _open_url_with_cmd(url)
    except (OSError, URLOpenError):
        _open_url_with_webbrowser(url)


class URLOpenError(Exception):
    pass


def _open_url_with_cmd(url: str):
    if sys.platform == "darwin":
        args = ["open", url]
    else:
        args = ["xdg-open", url]
    with open(os.devnull, "w") as null:
        try:
            subprocess.check_call(args, stderr=null, stdout=null)
        except subprocess.CalledProcessError as e:
            raise URLOpenError(url, e) from e


def _open_url_with_webbrowser(url: str):
    import webbrowser

    webbrowser.open(url)


LoopCallback = Callable[[], Any]
LoopWait = Callable[[float], Any]


def loop(
    cb: LoopCallback,
    wait: LoopWait,
    interval: int,
    first_interval: Optional[float] = None,
):
    try:
        _loop(cb, wait, interval, first_interval)
    except (Stop, KeyboardInterrupt):
        pass


def _loop(
    cb: LoopCallback, wait: LoopWait, interval: float, first_interval: Optional[float]
):
    loop_interval = first_interval if first_interval is not None else interval
    start = time.time()
    while True:
        sleep = _sleep_interval(loop_interval, start)
        loop_interval = interval
        should_stop = wait(sleep)
        if should_stop:
            break
        cb()


def _sleep_interval(interval: float, start: float):
    if interval <= 0:
        return 0
    now_ms = time.time_ns() // 1000000
    interval_ms = int(interval * 1000)
    start_ms = int(start * 1000)
    sleep_ms = (
        ((now_ms - start_ms) // interval_ms + 1) * interval_ms + start_ms - now_ms
    )
    return sleep_ms / 1000


class LoopingThread(threading.Thread):
    def __init__(
        self,
        cb: LoopCallback,
        interval: int,
        first_interval: Optional[float] = None,
        stop_timeout: float = 0,
    ):
        super().__init__()
        self._cb = cb
        self._interval = interval
        self._first_interval = first_interval
        self._stop_timeout = stop_timeout
        self._stop_event = threading.Event()
        self._stopped_event = threading.Event()

    def run(self):
        try:
            loop(
                cb=self._cb,
                wait=self._stop_event.wait,
                interval=self._interval,
                first_interval=self._first_interval,
            )
        finally:
            self._stopped_event.set()

    def stop(self):
        self._stop_event.set()
        self._stopped_event.wait(self._stop_timeout)


def safe_os_environ():
    return {
        name: val for name, val in os.environ.items() if name not in UNSAFE_OS_ENVIRON
    }


def match_filters(filters: list[str], vals: list[str], match_any: bool = False):
    test_fun = any if match_any else all
    vals_lower = [val.lower() for val in vals]
    filters_lower = [f.lower() for f in filters]
    return test_fun((any((f in val for val in vals_lower)) for f in filters_lower))


def split_description(s: str):
    lines = s.split("\n")
    return lines[0], _format_details(lines[1:])


def _format_details(details: list[str]):
    lines: list[str] = []
    for i, line in enumerate(details):
        if i > 0:
            lines.append("")
        lines.append(line)
    return lines


def parse_url(url: str):
    from urllib.parse import urlparse

    return urlparse(url)


class LogCapture(logging.Filter):
    def __init__(
        self,
        use_root_handler: bool = False,
        echo_to_stdout: bool = False,
        strip_ansi_format: bool = True,
        log_level: Optional[int] = None,
        other_loggers: Optional[list[logging.Logger]] = None,
    ):
        self._records: list[logging.LogRecord] = []
        self._use_root_handler = use_root_handler
        self._echo_to_stdout = echo_to_stdout
        self._strip_ansi_format = strip_ansi_format
        self._log_level = log_level
        self._other_loggers = other_loggers or []
        self._saved_log_levels = {}

    def __enter__(self):
        assert not self._saved_log_levels
        for logger in self._iter_loggers():
            logger.addFilter(self)
            self._apply_log_level(logger)
        self._records = []
        return self

    def __exit__(self, *exc: Any):
        for logger in self._iter_loggers():
            self._restore_log_level(logger)
            logger.removeFilter(self)
        self._saved_log_levels.clear()

    def _apply_log_level(self, logger: logging.Logger):
        if self._log_level is not None:
            self._saved_log_levels[logger] = logger.level
            logger.setLevel(self._log_level)

    def _restore_log_level(self, logger: logging.Logger):
        try:
            level = self._saved_log_levels[logger]
        except KeyError:
            pass
        else:
            logger.setLevel(level)

    def _iter_loggers(self):
        yield logging.root
        for logger in logging.Logger.manager.loggerDict.values():
            if isinstance(logger, logging.Logger):
                yield logger
        for logger in self._other_loggers:
            yield logger

    def filter(self, record: logging.LogRecord):
        self._records.append(record)
        if self._echo_to_stdout:
            sys.stdout.write(self._format_record(record))
            sys.stdout.write("\n")
        return False

    def _format_record(self, r: logging.LogRecord):
        msg = self._handler().format(r)
        if self._strip_ansi_format:
            msg = ansi_util.strip_ansi(msg)
        return msg

    def print_all(self):
        for r in self._records:
            sys.stdout.write(self._format_record(r))
            sys.stdout.write("\n")

    def _handler(self):
        if self._use_root_handler:
            return logging.root.handlers[0]

        return loglib.ConsoleLogHandler()

    def get_all(self):
        return self._records


def format_timestamp(ts: float, fmt: Optional[str] = None):
    if not ts:
        return ""
    dt = datetime.datetime.fromtimestamp(ts / 1000000)
    return dt.strftime(fmt or "%Y-%m-%d %H:%M:%S")


def utcformat_timestamp(ts: float, fmt: Optional[str] = None):
    if not ts:
        return None
    dt = datetime.datetime.utcfromtimestamp(ts / 1000000)
    return dt.strftime(fmt or "%Y-%m-%d %H:%M:%S UTC")


_raise_error_marker = object()


def resolve_refs(val: Any, kv: dict[str, Any], undefined: Any = _raise_error_marker):
    return _resolve_refs_recurse(val, kv, undefined, [])


def resolve_all_refs(kv: dict[str, Any], undefined: Any = _raise_error_marker):
    return {
        name: _resolve_refs_recurse(kv[name], kv, undefined, []) for name in sorted(kv)
    }


def _resolve_refs_recurse(
    val: Any, kv: dict[str, Any], undefined: Any, stack: list[str]
) -> str:
    if not isinstance(val, str):
        return val
    parts = [part for part in REF_P.split(val) if part != ""]
    resolved = list(_iter_resolved_ref_parts(parts, kv, undefined, stack))
    if len(resolved) == 1:
        return resolved[0]
    return "".join([_resolved_part_str(part) for part in resolved])


def _resolved_part_str(part: Any):
    if isinstance(part, str):
        return part
    from . import yaml_util  # expensive

    return yaml_util.encode_yaml(part)


def resolve_rel_paths(kv: dict[str, Any]):
    return {name: _resolve_rel_path(kv[name]) for name in kv}


def _resolve_rel_path(maybe_path: str):
    if os.path.exists(maybe_path) and not os.path.isabs(maybe_path):
        return os.path.abspath(maybe_path)
    return maybe_path


class ReferenceResolutionError(Exception):
    pass


class ReferenceCycleError(ReferenceResolutionError):
    pass


class UndefinedReferenceError(ReferenceResolutionError):
    def __init__(self, reference: Any):
        super().__init__(reference)
        self.reference = reference


def _iter_resolved_ref_parts(
    parts: list[str], kv: dict[str, Any], undefined: Any, stack: list[str]
):
    for part in parts:
        if part.startswith("${") and part.endswith("}"):
            ref_name = part[2:-1]
            if ref_name in stack:
                raise ReferenceCycleError(stack + [ref_name])
            stack.append(ref_name)
            ref_val = kv.get(ref_name, undefined)
            if ref_val is _raise_error_marker:
                raise UndefinedReferenceError(ref_name)
            yield _resolve_refs_recurse(ref_val, kv, undefined, stack)
            stack.pop()
        elif part.startswith("\\${") and part.endswith("}"):
            yield part[1:]
        else:
            yield part


def getmtime(filename: str):
    try:
        return os.path.getmtime(filename)
    except OSError:
        return None


def kill_process_tree(pid: int, force: bool = False, timeout: Optional[float] = None):
    import psutil
    import signal

    sig = signal.SIGKILL if force else signal.SIGTERM

    def send_sig(proc: psutil.Process):
        try:
            proc.send_signal(sig)
        except psutil.NoSuchProcess:
            pass

    root = psutil.Process(pid)
    children = root.children(recursive=True)
    send_sig(root)
    if not force:
        # Give children an opportunity to respond to signal
        psutil.wait_procs(children, timeout=5)
    for proc in children:
        send_sig(proc)
    return psutil.wait_procs([root] + children, timeout=timeout)


def safe_mtime(path: str):
    try:
        return os.path.getmtime(path)
    except OSError:
        return None


def safe_list_remove(x: Any, l: list[Any]):
    safe_list_remove_all([x], l)


def safe_list_remove_all(xs: list[Any], l: list[Any]):
    for x in xs:
        try:
            l.remove(x)
        except ValueError:
            pass


def local_server_url(host: str, port: int):
    import socket

    if not host or host == "0.0.0.0":
        host = socket.gethostname()
        try:
            # Verify that configured hostname is valid
            socket.gethostbyname(host)
        except socket.gaierror:
            host = "localhost"
    return f"http://{host}:{port}"


def format_duration(start_time: float, end_time: Optional[float] = None):
    """Returns formatted H:MM:SS time for start and end in microseconds."""
    if start_time is None:
        return None
    if end_time is None:
        end_time = time.time() * 1000000
    seconds = int((end_time - start_time) // 1000000)
    m, s = divmod(seconds, 60)
    h, m = divmod(m, 60)
    return f"{h}:{m:02d}:{s:02d}"


def format_dir(dir: str):
    return format_user_dir(os.path.abspath(dir))


def format_user_dir(s: str):
    if os.name == "nt":
        return s
    user_dir = os.path.expanduser("~")
    if s.startswith(user_dir):
        return os.path.join("~", s[len(user_dir) + 1 :])
    return s


def apply_env(target: dict[str, str], source: dict[str, str], names: list[str]):
    for name in names:
        try:
            target[name] = source[name]
        except KeyError:
            pass


def wait_forever(sleep_interval: float = 0.1):
    while True:
        time.sleep(sleep_interval)


def gpu_available():
    import ctypes

    if "linux" in sys.platform:
        lib = "libcublas.so"
    elif sys.platform == "darwin":
        lib = "libcublas.dylib"
    elif sys.platform == "win32":
        lib = "cublas.dll"
    else:
        log.warning("unable to detect GPU for platform '%s'", sys.platform)
        lib = None
    if lib:
        log.debug("checking for GPU by loading %s", lib)
        try:
            ctypes.CDLL(lib)
        except OSError as e:
            log.debug("error loading '%s': %s", lib, e)
        else:
            log.debug("%s loaded", lib)
            return True
    return False


def get_env(name: str, type: Callable[[Any], Any], default: Any = None):
    try:
        val = os.environ[name]
    except KeyError:
        return default
    else:
        try:
            return type(val)
        except Exception as e:
            log.warning("error converting env %s to %s: %s", name, type, e)
            return default


def del_env(names: list[str]):
    for name in names:
        try:
            del os.environ[name]
        except KeyError:
            pass


def is_executable_file(path: str):
    return os.path.isfile(path) and os.access(path, os.X_OK)


def hostname():
    return os.getenv("HOST") or _real_hostname()


def _real_hostname():
    import socket

    try:
        return socket.gethostname()
    except Exception:
        if log.getEffectiveLevel() <= logging.DEBUG:
            log.exception("socket.gethostname()")
        return ""


def user():
    return os.getenv("USER") or ""


def format_bytes(n: float):
    units = [None, "K", "M", "G", "T", "P", "E", "Z"]
    for unit in units[:-1]:
        if abs(n) < 1024:
            if not unit:
                return str(n)
            return f"{n:.1f}{unit}"
        n /= 1024.0
    return f"{n:.1f}{units[-1]}"


def platform_info():
    """Returns a dict of system info."""
    info = _platform_base_info()
    info.update(_platform_psutil_info())
    return info


def _platform_base_info():
    import platform

    return {
        "architecture": " ".join(platform.architecture()),
        "processor": platform.processor(),
        "python_version": sys.version.replace("\n", ""),
        "uname": " ".join(platform.uname()),
    }


def _platform_psutil_info() -> dict[str, Any]:
    try:
        import psutil
    except ImportError:
        return {}
    else:
        return {
            "cpus": psutil.cpu_count(),
        }


def gage_user_agent():
    import platform
    import gage

    system, _node, release, _ver, machine, _proc = platform.uname()
    return f"python-gage/{gage.__version__} ({system}; {machine}; {release})"


def apply_nested_config(kv: dict[str, Any], config: dict[str, Any]):
    for name, val in kv.items():
        _apply_nested_config(name, val, config)


def _apply_nested_config(name: str, val: Any, config: dict[str, Any]):
    name, parent = _nested_config_dest(name, config)
    parent[name] = val


def _nested_config_dest(
    name: str, config: dict[str, Any], nested_name_prefix: str = ""
) -> tuple[str, dict[str, Any]]:
    """Returns a tuple of name and dict as dest for a named value.

    `name` may contain dots, which are used to locate the destination
    in `config`.

    Follows a path from `name`, delimited by dots, to either find a
    matching dot-named entry in `config` or a nest entry in
    `config`. If neither a dot-named entry nor a nested entry exists
    in `config`, implicitly creates a nested entry.

    For examples, see *Applying values to existing configuration* in
    `guild/tests/util.md`.
    """
    assert isinstance(config, dict), config
    for name_trial in _iter_dot_name_trials(name):
        try:
            val = config[name_trial]
        except KeyError:
            pass
        else:
            attr_name = name[len(name_trial) + 1 :]
            if not attr_name:
                return name_trial, config
            if not isinstance(val, dict):
                raise ValueError(
                    f"'{nested_name_prefix}{name}' cannot be nested: conflicts with "
                    f"{{'{nested_name_prefix}{name_trial}': {val}}}"
                )
            return _nested_config_dest(attr_name, val, name_trial + ".")
    return _ensure_nested_dest(name, config)


def _iter_dot_name_trials(name: str):
    while True:
        yield name
        parts = name.rsplit(".", 1)
        if len(parts) == 1:
            break
        name = parts[0]


def _ensure_nested_dest(name: str, data: dict[str, Any]):
    name_parts = name.split(".")
    for i in range(len(name_parts) - 1):
        data = data.setdefault(name_parts[i], {})
        assert isinstance(data, dict), (name, data)
    return name_parts[-1], data


def encode_nested_config(config: dict[str, Any]) -> dict[str, Any]:
    encoded = {}
    for name, val in config.items():
        _apply_nested_encoded(name, val, [], encoded)
    return encoded


def _apply_nested_encoded(
    name: str, val: Any, parents: list[str], encoded: dict[str, Any]
):
    key_path = parents + [name]
    if isinstance(val, dict) and val:
        for item_name, item_val in val.items():
            _apply_nested_encoded(item_name, item_val, key_path, encoded)
    else:
        encoded[".".join(key_path)] = val


def short_digest(s: str):
    if not s:
        return ""
    return s[:8]


class HTTPResponse:
    def __init__(self, resp: Any):
        self.status_code = resp.status
        self.text = resp.read()


class HTTPConnectionError(Exception):
    pass


def http_post(url: str, data: dict[str, Any], timeout: Optional[float] = None):
    headers = {
        "User-Agent": gage_user_agent(),
        "Content-type": "application/x-www-form-urlencoded",
    }
    return _http_request(url, headers, data, "POST", timeout)


def http_get(url: str, timeout: Optional[float] = None):
    return _http_request(url, timeout=timeout)


def _http_request(
    url: str,
    headers: Optional[dict[str, str]] = None,
    data: Optional[dict[str, str]] = None,
    method: str = "GET",
    timeout: Optional[float] = None,
):
    from urllib.parse import urlencode, urlparse
    import socket

    headers = headers or {}
    url_parts = urlparse(url)
    conn = _HTTPConnection(url_parts.scheme, url_parts.netloc, timeout)
    params = urlencode(data) if data else ""
    try:
        conn.request(method, url_parts.path, params, headers)
    except socket.error as e:
        if e.errno == errno.ECONNREFUSED:
            raise HTTPConnectionError(url) from e
        raise
    else:
        return HTTPResponse(conn.getresponse())


def _HTTPConnection(scheme: str, netloc: str, timeout: Optional[float]):
    from http import client as http_client

    if scheme == "http":
        return http_client.HTTPConnection(netloc, timeout=timeout)
    if scheme == "https":
        return http_client.HTTPSConnection(netloc, timeout=timeout)
    raise ValueError(f"unsupported scheme '{scheme}' - must be 'http' or 'https'")


class StdIOContextManager:
    def __init__(self, stream: IO[AnyStr]):
        self.stream = stream

    def __enter__(self):
        return self.stream

    def __exit__(self, *exc: Any):
        pass


def check_env(env: dict[str, Any]):
    for name, val in env.items():
        if not isinstance(name, str):
            raise ValueError(f"non-string env name {name!r}")
        if not isinstance(val, str):
            raise ValueError(f"non-string env value for '{name}': {val!r}")


class SysArgv:
    def __init__(self, args: list[str]):
        self._args = args
        self._save = None

    def __enter__(self):
        assert self._save is None, self._save
        self._save = sys.argv[1:]
        sys.argv[1:] = self._args

    def __exit__(self, *exc: Any):
        assert self._save is not None
        sys.argv[1:] = self._save
        self._save = None


class Env:
    def __init__(self, vals: dict[str, str], replace: bool = False):
        self._vals = vals
        self._replace = replace
        self._revert_ops = []
        self._save_env = None

    def __enter__(self):
        if self._replace:
            self._replace_env()
        else:
            self._merge_env()

    def _replace_env(self):
        self._save_env = dict(os.environ)
        os.environ.clear()
        os.environ.update(self._vals)

    def _merge_env(self):
        env = os.environ
        for name, val in self._vals.items():
            try:
                cur = env.pop(name)
            except KeyError:
                self._revert_ops.append(self._del_env_fun(name, env))
            else:
                self._revert_ops.append(self._set_env_fun(name, cur, env))
            env[name] = val

    @staticmethod
    def _del_env_fun(name: str, env: "os._Environ[str]"):
        def f():
            try:
                del env[name]
            except KeyError:
                pass

        return f

    @staticmethod
    def _set_env_fun(name: str, val: str, env: "os._Environ[str]"):
        def f():
            env[name] = val

        return f

    def __exit__(self, *exc: Any):
        if self._replace:
            self._restore_env()
        else:
            self._unmerge_env()

    def _restore_env(self):
        assert self._save_env is not None
        os.environ.clear()
        os.environ.update(self._save_env)

    def _unmerge_env(self):
        for op in self._revert_ops:
            op()


class StdinReader:
    def __init__(self, stop_on_blank_line: bool = False):
        self.stop_on_blank_line = stop_on_blank_line

    __enter__ = lambda self, *_args: self
    __exit__ = lambda *_args: None

    def __iter__(self):
        for line in sys.stdin:
            line = line.rstrip()
            if not line and self.stop_on_blank_line:
                break
            yield line


def env_var_name(s: str):
    return re.sub("[^A-Z0-9_]", "_", s.upper())


def env_var_quote(s: str):
    if s == "":
        return ""
    return shlex_util.shlex_quote(s)


def bind_method(obj: Any, method_name: str, function: Any):
    setattr(obj, method_name, function.get(obj, obj.__class__))


def edit(
    s: str = "",
    extension: str = ".txt",
    strip_comments_prefix: str = "",
):
    import click

    try:
        edited = click.edit(s, _try_editor(), extension=extension)
    except click.UsageError as e:
        raise ValueError(e) from e
    else:
        if edited is None:
            return None
        return (
            _strip_comment_lines(edited, strip_comments_prefix)
            if strip_comments_prefix
            else edited
        )


def _try_editor():
    return find_apply([_try_editor_env, _try_editor_bin])


def _try_editor_env():
    names = ("VISUAL", "EDITOR")
    for name in names:
        val = os.getenv(name)
        if val:
            return val
    return None


def _try_editor_bin():
    """Returns /usr/bin/editor if it exists.

    This is the path configured by `update-alternatives` on Ubuntu
    systems.
    """
    editor_bin = "/usr/bin/editor"
    if os.path.exists(editor_bin):
        return editor_bin
    return None


def _strip_comment_lines(s: str, prefix: str):
    return "\n".join([line for line in s.split("\n") if line.rstrip()[:1] != prefix])


PropertyCacheProp = tuple[str, Any, Callable[..., Any], float]


class PropertyCache:
    def __init__(self, properties: list[PropertyCacheProp]):
        self._vals = {
            name: default for name, default, _callback, _timeout in properties
        }
        self._expirations = {
            name: 0.0 for name, _default, _callback, _timeout in properties
        }
        self._timeouts = {
            name: timeout for name, _default, _callback, timeout in properties
        }
        self._callbacks = {
            name: callback for name, _default, callback, _timeout in properties
        }

    def get(self, name: str, *args: Any, **kw: Any):
        if time.time() < self._expirations[name]:
            return self._vals[name]
        val = self._callbacks[name](*args, **kw)
        self._vals[name] = val
        self._expirations[name] = time.time() + self._timeouts[name]
        return val


def natsorted(*args: Any, **kw: Any):
    from natsort import natsorted as ns

    return ns(*args, **kw)


class lazy_str:
    def __init__(self, f: Callable[[], str]):
        self.f = f

    def __str__(self):
        return self.f()


_KNOWN_SHELLS = (
    "bash",
    "cmd",
    "code",
    "dash",
    "fish",
    "powershell",
    "pwsh",
    "sh",
    "xonsh",
    "zsh",
)
_cached_active_shell = "__unset__"


def active_shell():
    if _cached_active_shell != "__unset__":
        return _cached_active_shell
    shell = _active_shell()
    globals()["_cached_active_shell"] = shell
    return shell


def _active_shell():
    import psutil

    p = psutil.Process().parent()
    while p:
        p_name = _shell_for_proc(p)
        if p_name in _KNOWN_SHELLS:
            return p_name
        p = p.parent()
    return None


_Process = Any  # proxy for psutil.Process


def _shell_for_proc(p: _Process):
    name = p.name()
    if name.endswith(".exe"):
        return name[:-4]
    return name


class StderrCapture:
    closed = False
    _stderr = None
    _captured = []

    def __init__(self, autoprint: bool = False):
        self._autoprint = autoprint

    def __enter__(self):
        self._stderr = sys.stderr
        self._captured = []
        self.closed = False
        sys.stderr = self
        return self

    def __exit__(self, *exc: Any):
        assert self._stderr is not None
        sys.stderr = self._stderr
        self.closed = True

    def write(self, s: str):
        self._captured.append(s)
        if self._autoprint:
            sys.stdout.write(s)
            sys.stdout.flush()

    def flush(self):
        pass

    def print(self):
        for part in self._captured:
            sys.stdout.write(self._decode_part(part))
        sys.stdout.flush()

    def get_value(self):
        return "".join([self._decode_part(part) for part in self._captured])

    @staticmethod
    def _decode_part(part: Any):
        return part.decode("utf-8") if hasattr(part, "decode") else part


class StdoutCapture:
    closed = False
    _stdout = None
    _captured = []

    def __enter__(self):
        self._stdout = sys.stdout
        self._captured = []
        self.closed = False
        sys.stdout = self
        return self

    def __exit__(self, *exc: Any):
        assert self._stdout is not None
        sys.stdout = self._stdout
        self.closed = True

    def write(self, b: bytes):
        self._captured.append(b)

    def flush(self):
        pass

    def get_value(self):
        return "".join(self._captured)


def check_gage_version(req: str):
    import gage
    from . import python_util

    return python_util.check_package_version(gage.__version__, req)


def split_lines(s: str):
    return [line for line in re.split(r"\r?\n", s) if line]


def dict_to_camel_case(d: dict[str, Any]):
    return {to_camel_case(k): v for k, v in d.items()}


def to_camel_case(s: str):
    parts = tokenize_snake_case_for_camel_case(s)
    in_upper = False
    for i, part in enumerate(parts):
        if part == "":
            parts[i] = "_"
        elif not in_upper:
            parts[i] = part
            in_upper = True
        else:
            parts[i] = f"{part[0].upper()}{part[1:]}"
    return "".join(parts)


def tokenize_snake_case_for_camel_case(s: str):
    under_split = s.split("_")
    # If all underscores, remove extra token
    if not any(iter(under_split)):
        return under_split[1:]
    return under_split


T = TypeVar("T")


def flatten(l: list[list[T]]) -> list[T]:
    return [item for sublist in l for item in sublist]


def try_env(name: str, cvt: Optional[Callable[[str], Any]] = None):
    val_str = os.getenv(name)
    if val_str is None or cvt is None:
        return None
    try:
        return cvt(val_str)
    except (TypeError, ValueError):
        return None


def decode_cfg_val(s: str) -> int | float | bool | str:
    for conv in [int, float, _cfg_bool]:
        try:
            return conv(s)
        except ValueError:
            pass
    return s


def _cfg_bool(s: str):
    import configparser

    try:
        return configparser.ConfigParser.BOOLEAN_STATES[s.lower()]
    except KeyError:
        raise ValueError() from None


def encode_cfg_val(x: Any):
    return str(x)


def which(cmd: str):
    which_cmd = "where" if os.name == "nt" else "which"
    devnull = open(os.devnull, "w")
    try:
        out = subprocess.check_output([which_cmd, cmd], stderr=devnull)
    except subprocess.CalledProcessError:
        return None
    else:
        assert out, cmd
        return out.decode("utf-8").split(os.linesep, 1)[0]


T = TypeVar("T")


def coerce_list(x: T | list[T]) -> list[T]:
    return x if isinstance(x, list) else [x]
