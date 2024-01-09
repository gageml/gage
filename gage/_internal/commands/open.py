# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option

RunSpec = Annotated[
    str,
    Argument(
        metavar="[run]",
        help=(
            "Run to show information for. Value may be an index "
            "number, run ID, or run name. Default is latest run."
        ),
    ),
]

Path = Annotated[
    str,
    Option(
        "-p",
        "--path",
        metavar="path",
        help="Run file to open. Use '[cmd]gage show --files[/]' to show run files.",
    ),
]

Cmd = Annotated[
    str,
    Option(
        "-c",
        "--cmd",
        metavar="cmd",
        help="System command to use. Default is the program associated with the path.",
    ),
]

MetaFlag = Annotated[bool, Option("--meta", hidden=True)]
UserFlag = Annotated[bool, Option("--user", hidden=True)]
SummaryFlag = Annotated[bool, Option("--summary", hidden=True)]


def open(
    run: RunSpec = "",
    path: Path = "",
    cmd: Cmd = "",
    meta: MetaFlag = False,
    user: UserFlag = False,
    summary: SummaryFlag = False,
):
    """Open a run or run file.

    The default system program is used to open the specified path, which
    is the run directory by default. Use `--path` to specify a relative
    file path. Use `--cmd` to open the file with an alternative program.
    """

    from .open_impl import open, Args

    open(Args(run, path, cmd, meta, user, summary))
