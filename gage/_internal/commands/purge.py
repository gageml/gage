# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Context

from ..cli import Argument
from ..cli import Option

RunSpecs = Annotated[
    Optional[list[str]],
    Argument(
        help="Runs to remove. Required unless '--all' is specified.",
        metavar="[run]...",
        show_default=False,
        incompatible_with=["all"],
    ),
]

Archive = Annotated[
    str,
    Option(
        "-A",
        "--archive",
        metavar="name",
        help="Permanently delete archived runs.",
    ),
]

Where = Annotated[
    str,
    Option(
        "-w",
        "--where",
        metavar="expr",
        help="Select runs matching filter expression.",
    ),
]

AllFlag = Annotated[
    bool,
    Option(
        "-a",
        "--all",
        help="Remove all deleted runs.",
    ),
]

YesFlag = Annotated[
    bool,
    Option(
        "-y",
        "--yes",
        help="Permanently delete runs without prompting.",
    ),
]


def runs_purge(
    ctx: Context,
    runs: RunSpecs = None,
    archive: Archive = "",
    where: Where = "",
    all: AllFlag = False,
    yes: YesFlag = False,
):
    """Permanently delete runs.

    Use to remove deleted or archived runs, freeing disk space. Note
    that purged runs cannot be recovered.

    Use [cmd]gage list --deleted[/] to list deleted runs that can be
    removed.

    Use [cmd]gage list --archive NAME[/] to list archived runs that can
    be removed.
    """
    from .purge_impl import runs_purge, Args

    runs_purge(Args(ctx, runs or [], archive, where, all, yes))
