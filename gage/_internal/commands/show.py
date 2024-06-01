# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..cli import Argument
from ..cli import Option

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

ConfigFlag = Annotated[
    bool,
    Option(
        "-c",
        "--config",
        help=("Show only config."),
        incompatible_with=["files", "output", "summary"],
    ),
]

SummaryFlag = Annotated[
    bool,
    Option(
        "-s",
        "--summary",
        help=("Show only summary."),
        incompatible_with=["files", "output"],
    ),
]

OutputFlag = Annotated[
    bool,
    Option(
        "-o",
        "--output",
        help=("Show only output."),
        incompatible_with=["summary", "files"],
    ),
]


FilesFlag = Annotated[
    bool,
    Option(
        "-f",
        "--files",
        help="Show only files. When used, all files are show.",
        incompatible_with=["summary", "output"],
    ),
]


def show(
    run: RunSpec = "",
    where: Where = "",
    limit_files: LimitFiles = 40,
    all_files: AllFilesFlag = False,
    config: ConfigFlag = False,
    summary: SummaryFlag = False,
    output: OutputFlag = False,
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
            config,
            summary,
            output,
            files,
        )
    )
