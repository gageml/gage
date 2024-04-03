# SPDX-License-Identifier: Apache-2.0

from typing import *

import os

from ...__init__ import __version__

from .. import cli
from .. import sys_config
from .. import log
from .. import var


class Args(NamedTuple):
    version: bool
    cwd: str
    runs_dir: str
    verbose: int


def main(args: Args):
    _init_logging(args)
    _init_runs_dir(args)
    if args.version:
        cli.out(f"Gage {__version__}")
        raise SystemExit(0)
    if args.cwd:
        _apply_cwd(args.cwd)


def _init_runs_dir(args: Args):
    if args.runs_dir:
        var.set_runs_dir(args.runs_dir)


def _init_logging(args: Args):
    log_level = (
        log.DEBUG if args.verbose >= 2 else log.INFO if args.verbose == 1 else log.WARN
    )
    log.init_logging(log_level)


def _apply_cwd(cwd: str):
    sys_config.set_cwd(_validated_dir(cwd))


def _validated_dir(path: str):
    path = os.path.expanduser(path)
    if not os.path.exists(path):
        raise SystemExit(f"directory '{path}' does not exist")
    if not os.path.isdir(path):
        raise SystemExit(f"'{path}' is not a directory")
    return path
