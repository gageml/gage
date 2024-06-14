# SPDX-License-Identifier: Apache-2.0

from typing import *

import os
import re
import shlex

__all__ = [
    "shlex_split",
    "shlex_quote",
    "shlex_join",
    "split_env",
]


def shlex_split(s: str):
    # If s is None, this call will block (see
    # https://bugs.python.org/issue27775)
    s = s or ""
    return _shlex_split_windows(s) if os.name == "nt" else _shlex_split_posix(s)


def _shlex_split_windows(s: str):
    assert s is not None  # proc hangs if None
    return [_unquote(s) for s in shlex.split(s, posix=False)]


def _unquote(s: str):
    if s[:1] == "'" and s[-1:] == "'":
        return s[1:-1]
    if s[:1] == "\"" and s[-1:] == "\"":
        return s[1:-1]
    return s


def _shlex_split_posix(s: str):
    assert s is not None  # proc hangs if None
    return shlex.split(s, posix=True)


def shlex_join(args: list[str]):
    return " ".join([shlex_quote(arg) for arg in args])


def shlex_quote(s: str):
    if os.name == "nt":
        return _shlex_quote_windows(s)
    return _simplify_shlex_quote(shlex.quote(s))


_find_windows_unsafe = re.compile(r"[^\w@%+=:,./\\-]", re.ASCII).search


def _shlex_quote_windows(s: str):
    if not s:
        return "\"\""
    if _find_windows_unsafe(s) is None:
        return s
    return "\"" + s.replace("\"", "\\\"") + "\""


def _simplify_shlex_quote(s: str):
    repls = [
        ("''\"'\"'", "\"'"),
    ]
    for pattern_start, repl_start in repls:
        if not s.startswith(pattern_start):
            continue
        pattern_end = "".join(reversed(pattern_start))
        if not s.endswith(pattern_end):
            continue
        repl_end = "".join(reversed(repl_start))
        stripped = s[len(pattern_start) : -len(pattern_end)]
        return repl_start + stripped + repl_end
    return s


_ENV_ASSIGN_P = re.compile(r"([\w]+)=(.*)")


def split_env(cmd: str) -> tuple[str, dict[str, str]]:
    if not cmd:
        return cmd, {}
    parts = shlex.split(cmd)
    env = {}
    for i, part in enumerate(parts):
        m = _ENV_ASSIGN_P.match(part)
        if not m:
            break
        env[m.group(1)] = _unquote(m.group(2))
    return shlex_join(parts[i:]), env
