# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option

from .. import cli


RunSpec = Annotated[
    str,
    Argument(
        metavar="[run]",
        help=(
            "Run to show information for. Value may be an index "
            "number, run ID, or run name."
        ),
    ),
]

Where = Annotated[
    str,
    Option(
        "-w",
        "--where",
        metavar="expr",
        help="Limit available runs to show.",
    ),
]

LimitFiles = Annotated[
    int,
    Option(
        "--limit-files",
        metavar="N",
        help=(
            "Limit files shown. Default is 50. Ignored if --files "
            "is specified. Use --all-files to bypass this limit."
        ),
    ),
]

AllFilesFlag = Annotated[
    bool,
    Option(
        "--all-files",
        help="Show all files. --limit-files is ignored.",
    ),
]

SummaryFlag = Annotated[
    bool,
    Option(
        "--summary",
        help=("Show only summary."),
        callback=cli.incompatible_with("files"),
    ),
]


FilesFlag = Annotated[
    bool,
    Option(
        "-f",
        "--files",
        help="Show only files. When used, all files are show.",
        callback=cli.incompatible_with("summary"),
    ),
]


def show(
    run: RunSpec = "",
    where: Where = "",
    limit_files: LimitFiles = 40,
    all_files: AllFilesFlag = False,
    summary: SummaryFlag = False,
    files: FilesFlag = False,
):
    """Show information about a run."""
    from .show_impl import show, Args

    show(
        Args(
            run,
            where,
            limit_files,
            all_files,
            summary,
            files,
        )
    )
