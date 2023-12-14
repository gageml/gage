# SPDX-License-Identifier: Apache-2.0

from typing import *

import os
import threading

__all__ = [
    "cwd",
    "get_user",
    "get_runs_home",
    "set_cwd",
    "set_runs_home",
]


__cwd_lock = threading.Lock()
__cwd = None


USER_HOME = os.path.expanduser("~")


def set_cwd(cwd: Optional[str]):
    """Sets configured current directory.

    The configured current directory is different from `os.getcwd()` as
    it's specified by the user (or a user proxy) as a command-specific
    current directory.

    gage maintains the distinction between `cwd` and the process
    current directory to provide meaningful user messages. If changes to
    the configured current directory were applied as `os.chdir()`,
    messages to users would no longer reflect the user-facing current
    directory.

    Consider this command:

        $ gage -C my-project run train

    To the user, this is equivalent to `cd my-project; gage run train`.
    However, gage can't use this technique internally to run the
    command because it needs to preserve the user-facing cwd for
    messages.
    """
    with __cwd_lock:
        globals()["__cwd"] = cwd


def cwd():
    """Returns the configured current directory.

    If a configured directory is not set, returns `os.getcwd()`.

    See `set_cwd()` for details on the configured current directory vs
    the process current directory (i.e. `os.getcwd()`).
    """
    with __cwd_lock:
        return __cwd or os.getcwd()


class SetCwd:
    _save = None

    def __init__(self, path: str):
        self.path = path

    def __enter__(self):
        self._save = cwd()
        set_cwd(self.path)

    def __exit__(self, *args: Any):
        set_cwd(self._save)


def set_runs_home(dirname: str):
    os.environ["GAGE_RUNS"] = dirname


def get_runs_home():
    return os.getenv("GAGE_RUNS") or os.getenv("RUNS_HOME") or default_runs_home()


def default_runs_home():
    return os.path.join(USER_HOME, ".gage", "runs")


def get_user():
    # TODO: scheme should support optional verification, so we'll need
    # to take in house with likely support for open auth style user ID
    import getpass

    return getpass.getuser()
