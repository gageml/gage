# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import logging
import os
import subprocess
import sys

from .. import cli
from .. import run_meta

from ..run_attr import run_user_dir
from ..shlex_util import shlex_split

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
    args = _apply_summary_option(args)
    run = one_run(args)
    if args.meta and run_meta.is_zip(run.meta_dir):
        _open(run.meta_dir, args)
    else:
        _open(_path(run, args), args)
    _flush_streams_and_exit()


def _apply_summary_option(args: Args):
    if args.summary:
        if args.path:
            cli.exit_with_error("summary and path cannot be used together")
        return args._replace(path="summary.json", meta=True)
    return args


def cat(args: Args):
    args = _apply_summary_option(args)
    if not args.path:
        _path_required_for_cat_error(args)
    run = one_run(args)
    if args.meta:
        _cat_meta_file(run, args)
    else:
        _cat_dir_file(run, args)


def _cat_meta_file(run: Run, args: Args):
    assert args.path
    try:
        f = run_meta.open_meta_file(run, args.path, text=False)
    except FileNotFoundError as e:
        _path_not_found_error(e.filename)
    except KeyError:
        _zip_entry_not_found(run.meta_dir, args.path)
    else:
        with f:
            sys.stdout.buffer.write(f.read())


def _zip_entry_not_found(filename: str, name: str):
    cli.exit_with_error(
        f"cannot open \"{name}\" in \"{filename}\": entry does not exist"
    )


def _cat_dir_file(run: Run, args: Args):
    path = _path(run, args)
    if not os.path.exists(path):
        _path_not_found_error(path)
    if not os.path.isfile(path):
        _path_not_file_error(path)
    _cat(_path(run, args))


def _path_required_for_cat_error(args: Args) -> NoReturn:
    run_spec = f"{args.run} " if args.run else ""
    cli.exit_with_error(
        "Specify the file to print using '--path'. Use "
        f"'[cmd]gage show {run_spec}--files[/]' to show available files."
    )


def _path_not_found_error(path: str) -> NoReturn:
    cli.exit_with_error(f"cannot open \"{path}\": no such file")


def _path_not_file_error(path: str) -> NoReturn:
    cli.exit_with_error(f"cannot open \"{path}\": not a file")


def _path(run: Run, args: Args):
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
    return _proc_f(args.cmd) if args.cmd else _default_open_f()


def _default_open_f():
    if os.name == "nt":
        return os.startfile
    elif sys.platform.startswith("darwin"):
        return _proc_f("open")
    elif os.name == "posix":
        return _proc_f("xdg-open")
    else:
        cli.exit_with_error(
            f"unsupported platform: {sys.platform} {os.name}\n"  # \
            "Try --cmd with a program."
        )


def _proc_f(prog: str):
    cmd = shlex_split(prog)

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
