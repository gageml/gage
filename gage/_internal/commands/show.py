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
            "number, run ID, or run name."
        ),
    ),
]


FilesFlag = Annotated[
    bool,
    Option(
        "-f",
        "--files",
        help="Show run files.",
    ),
]


def show(run: RunSpec = "", files: FilesFlag = False):
    """Show information about a run."""
    from .show_impl import show, Args

    show(Args(run, files))
