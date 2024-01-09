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

open_ = open


class Args(NamedTuple):
    run: str
    path: str
    cmd: str
    meta: bool = False
    user: bool = False
    summary: bool = False
    where: str = ""


def open(args: Args):
    run = one_run(args)
    _open(_path(run, args), args)
    _flush_streams_and_exit()


def cat(args: Args):
    if not args.path:
        _path_required_for_cat_error(args)
    run = one_run(args)
    path = _path(run, args)
    if not os.path.isfile(path):
        _path_not_file_error(path)
    _cat(_path(run, args))


def _path_required_for_cat_error(args: Args) -> NoReturn:
    run_spec = f"{args.run} " if args.run else ""
    cli.exit_with_error(
        "Specify the file to print using '--path'. Use "
        f"'[cmd]gage show {run_spec}--files[/]' to show available files."
    )


def _path_not_file_error(path: str) -> NoReturn:
    cli.exit_with_error(f"Path \"{path}\" is not a run file")


def _path(run: Run, args: Args):
    if args.summary:
        args = args._replace(path="summary.json", meta=True)
    dirname = _dirname(run, args)
    return os.path.join(dirname, args.path) if args.path else dirname


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
        try:
            subprocess.run(cmd + [path], check=True)
        except subprocess.CalledProcessError as e:
            raise SystemExit(e.returncode)

    return f


def _flush_streams_and_exit():
    sys.stdout.flush()
    sys.exit(0)


def _cat(path: str):
    with open_(path, "rb") as f:
        sys.stdout.buffer.write(f.read())
