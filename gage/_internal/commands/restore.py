# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Context

from ..cli import Argument
from ..cli import Option

RunSpecs = Annotated[
    Optional[list[str]],
    Argument(
        help="Runs to restore. Required unless '--all' is specified.",
        metavar="[run]...",
        show_default=False,
        incompatible_with=["all"],
    ),
]

Where = Annotated[
    str,
    Option(
        "-w",
        "--where",
        metavar="expr",
        help="Restore runs matching filter expression.",
    ),
]

AllFlag = Annotated[
    bool,
    Option(
        "-a",
        "--all",
        help="Restore all runs.",
    ),
]

Archive = Annotated[
    str,
    Option(
        "-A",
        "--archive",
        metavar="name",
        help="Restore runs from an archive.",
    ),
]

YesFlag = Annotated[
    bool,
    Option(
        "-y",
        "--yes",
        help="Restore runs without prompting.",
    ),
]


def runs_restore(
    ctx: Context,
    runs: RunSpecs = None,
    where: Where = "",
    archive: Archive = "",
    all: AllFlag = False,
    yes: YesFlag = False,
):
    """Restore deleted or archived runs.

    Use to restore deleted runs. Note that is a run is permanently
    deleted, it cannot be restored.

    Use [cmd]gage list --deleted[/] to list deleted runs that can be
    restored.

    If '--archive' is specified, restores runs from the archive. Use
    [cmd]gage archive --list[/] for a list of archive names.
    """
    from .restore_impl import runs_restore, Args

    runs_restore(Args(ctx, runs or [], where, archive, all, yes))
