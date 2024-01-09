# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option

RunSpec = Annotated[
    str,
    Argument(
        metavar="[run]",
        help=(
            "Run to show file for. Value may be an index "
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
        help="Run file to show. Use '[cmd]gage show --files[/]' to show run files.",
    ),
]

MetaFlag = Annotated[bool, Option("--meta", hidden=True)]
UserFlag = Annotated[bool, Option("--user", hidden=True)]
SummaryFlag = Annotated[bool, Option("--summary", hidden=True)]


def cat(
    run: RunSpec = "",
    path: Path = "",
    meta: MetaFlag = False,
    user: UserFlag = False,
    summary: SummaryFlag = False,
):
    """Show a run file."""

    from .open_impl import cat, Args

    cat(Args(run, path, "", meta, user, summary))
