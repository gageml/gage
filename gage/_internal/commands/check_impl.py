# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import os
import platform
import sys

import gage

from .. import cli
from .. import gagefile
from .. import project_util
from .. import sys_config
from .. import util
from .. import var

from . import error_handlers


__all__ = ["check"]

CheckData = list[tuple[str, str]]


class Args(NamedTuple):
    path: str
    version: str
    json: bool
    verbose: bool


def check(args: Args):
    if args.path:
        _check_gagefile_and_exit(args)
    if args.version:
        _check_version_and_exit(args)
    _print_check_info(args)


def _check_gagefile_and_exit(args: Args):
    filename = _gagefile_filename(args.path)
    data = _gagefile_data(filename, args)
    _validate_gagefile_data_and_exit(data, filename, args)


def _gagefile_filename(path: str):
    assert path
    if os.path.isdir(path):
        return _gagefile_path_for_dir(path)
    if os.path.exists(path):
        return path
    error_handlers.gagefile_find_error(path)


def _gagefile_path_for_dir(dirname: str):
    try:
        return gagefile.gagefile_path_for_dir(dirname)
    except FileNotFoundError:
        error_handlers.gagefile_find_error(dirname)


def _gagefile_data(filename: str, args: Args):
    try:
        return gagefile.load_gagefile_data(filename)
    except gagefile.GageFileLoadError as e:
        error_handlers.gagefile_load_error(e)


def _validate_gagefile_data_and_exit(data: Any, filename: str, args: Args):
    try:
        gagefile.validate_gagefile_data(data)
    except gagefile.GageFileValidationError as e:
        error_handlers.gagefile_validation_error(e, filename, args.verbose)
    else:
        cli.err(f"{filename} is a valid Gage file")
        raise SystemExit(0)


def _check_version_and_exit(args: Args):
    try:
        match = util.check_gage_version(args.version)
    except ValueError as e:
        _bad_version_spec_error(e)
    else:
        if not match:
            cli.exit_with_error(
                f"Version mismatch: current version '{gage.__version__}' "
                f"does not match '{args.version}'"
            )
        raise SystemExit(0)


def _bad_version_spec_error(e: ValueError):
    err_msg = e.args[0].split("\n")[0]
    cli.exit_with_error(
        f"{err_msg}\nSee https://bit.ly/45AerAj for help with version specs."
    )


def _print_check_info(args: Args):
    data = _check_info_data(args)
    if args.json:
        _print_check_info_json(data)
    else:
        _print_check_info_table(data)


def _check_info_data(args: Args):
    return _core_info_data() + _maybe_verbose_info_data(args.verbose)


def _core_info_data() -> CheckData:
    return [
        ("gage_version", gage.__version__),
        ("gage_install_location", gage.__pkgdir__),
        ("python_version", sys.version),
        ("python_exe", sys.executable),
        ("platform", platform.platform()),
    ]


def _maybe_verbose_info_data(verbose: bool) -> CheckData:
    if not verbose:
        return []
    cwd = sys_config.cwd()
    project_dir = project_util.find_project_dir(cwd)
    gagefile = _try_gagefile(project_dir or cwd)
    return [
        ("command_directory", cwd),
        ("project_directory", project_dir or "<none>"),
        ("gagefile", gagefile.filename if gagefile else "<none>"),
        ("runs_directory", var.runs_dir()),
    ]


def _try_gagefile(cwd: str):
    try:
        return gagefile.gagefile_for_dir(cwd)
    except FileNotFoundError:
        return None


def _print_check_info_json(data: CheckData):
    cli.out(cli.json({name: val for name, val in data}))


def _print_check_info_table(data: CheckData):
    table = cli.Table(show_header=False)
    for name, val in data:
        table.add_row(cli.label(name), val)
    cli.out(table)
