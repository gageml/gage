# SPDX-License-Identifier: Apache-2.0

from typing import *

import shlex

__all__ = [
    "shlex_split",
    "shlex_quote",
    "shlex_join",
]


def shlex_split(s: str):
    # If s is None, this call will block (see
    # https://bugs.python.org/issue27775)
    s = s or ""
    return shlex.split(s, posix=True)


def shlex_quote(s: str):
    return _simplify_shlex_quote(shlex.quote(s))


def shlex_join(args: list[str]):
    return " ".join([shlex_quote(arg) for arg in args])


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
