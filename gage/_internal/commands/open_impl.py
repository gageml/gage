# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import logging
import os
import shlex
import subprocess
import sys

from .. import cli

from ..run_util import run_user_dir

from .impl_support import one_run

log = logging.getLogger(__name__)


class Args(NamedTuple):
    run: str
    path: str
    cmd: str
    meta: bool
    user: bool


def open(args: Args):
    run = one_run(args)
    dirname = _dirname(run, args)
    path = os.path.join(dirname, args.path) if args.path else dirname
    _open(path, args)
    _flush_streams_and_exit()


def _dirname(run: Run, args: Args):
    if args.meta:
        return run.meta_dir
    if args.user:
        return run_user_dir(run)
    return run.run_dir


def _open(path: str, args: Args):
    try:
        _open_f(args)(path)
    except Exception as e:
        if log.getEffectiveLevel() <= logging.DEBUG:
            log.exception("opening %s", path)
        cli.exit_with_error(str(e))


def _open_f(args: Args):
    if args.cmd:
        return _proc_f(args.cmd)
    if os.name == "nt":
        return os.startfile
    if sys.platform.startswith("darwin"):
        return _proc_f("open")
    if os.name == "posix":
        return _proc_f("xdg-open")
    cli.exit_with_error(
        f"unsupported platform: {sys.platform} {os.name}\n"  # \
        "Try --cmd with a program."
    )


def _proc_f(prog: str):
    cmd = shlex.split(prog)

    def f(path: str):
        p = subprocess.Popen(cmd + [path])
        p.wait()

    return f


def _flush_streams_and_exit():
    sys.stdout.flush()
    sys.exit(0)
