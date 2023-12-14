# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

from .. import cli


def gagefile_error(e: GageFileError) -> NoReturn:
    if isinstance(e, FileNotFoundError):
        cli.err("No operations defined for the current directory")
        raise SystemExit()
    elif isinstance(e, GageFileLoadError):
        cli.exit_with_error(
            f"There was an issue reading operations from {e.filename}: {e.msg}"
        )
    else:
        assert False, e


def opdef_not_found(e: OpDefNotFound) -> NoReturn:
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
