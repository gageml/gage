# SPDX-License-Identifier: Apache-2.0

from typing import *

import logging
import os

from ..types import *

from .. import cli
from .. import gagefile
from .. import run_util

__ALL__ = [
    "gagefile_error",
    "gagefile_not_found",
    "gagefile_find_error",
    "gagefile_validation_error",
    "gagefile_load_error",
    "opdef_not_found",
    "missing_exec_error",
    "run_exec_error",
]

log = logging.getLogger(__name__)


def gagefile_error(e: GageFileError):
    cli.exit_with_error(
        f"There was an issue reading operations from {e.filename}: {e.msg}"
    )


def gagefile_not_found(e: FileNotFoundError) -> NoReturn:
    cli.exit_with_error("No operations defined for the current directory")


def gagefile_find_error(path: str) -> NoReturn:
    if os.path.isdir(path):
        list = "".join(["\n  " + name for name in gagefile.gagefile_candidates()])
        cli.exit_with_error(
            f"Cannot find a Gage file in {path}\n\n"
            f"Looking for one of:{list}\n\n"
            "For help with Gage files try 'gage help gagefile'"
        )
    else:
        cli.exit_with_error(f"File \"{path}\" does not exist")


def gagefile_validation_error(
    e: gagefile.GageFileValidationError, filename: str, verbose: bool = False
):
    import json
    from .. import schema_util

    cli.err(f"There are errors in {filename}")
    if verbose:
        output = schema_util.validation_error_output(e)
        cli.err(json.dumps(output, indent=2, sort_keys=True))
    else:
        for err in schema_util.validation_errors(e):
            cli.err(err)
    raise SystemExit(1)


def gagefile_load_error(e: gagefile.GageFileLoadError) -> NoReturn:
    cli.exit_with_error(f"Error loading {e.filename}: {e.msg}")


def opdef_not_found(e: OpDefNotFound):
    from .operations_impl import operations_table

    msg = (
        e.opspec
        and f"Cannot find operation '{e.opspec}'"
        or "Cannot find a default operation"
    )
    cli.err(msg)
    try:
        ops = operations_table()
    except FileNotFoundError:
        cli.err(
            "\nTo define operations, create a Gage file. Run "
            "'gage help gagefile' for more information."
        )
    else:
        cli.err("\nOperations defined for this project:\n")
        cli.err(ops)
    raise SystemExit()


def missing_exec_error(ctx: RunContext) -> NoReturn:
    cli.exit_with_error(
        f"Operation {ctx.opdef.name} defined in \"{ctx.gagefile.filename}\" "
        "is missing an exec command\n"
        "Run 'gage help exec' for more information."
    )


def run_exec_error(e: run_util.RunExecError):
    log.debug(
        "Exec error for phase %r command %r exit %r",
        e.phase_name,
        e.proc_args,
        e.exit_code,
    )
    raise SystemExit(e.exit_code)
