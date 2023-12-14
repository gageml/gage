# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import *

import click

from .. import cli


RunSpecs = Annotated[
    Optional[list[str]],
    Argument(
        help="Runs to restore. Required unless '--all' is specified.",
        metavar="[run]...",
        show_default=False,
        callback=cli.incompatible_with("all"),
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
        help="Restore all deleted runs.",
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
    ctx: click.Context,
    runs: RunSpecs = None,
    where: Where = "",
    all: AllFlag = False,
    yes: YesFlag = False,
):
    """Restore runs.

    Use to restore deleted runs. Note that is a run is permanently
    deleted, it cannot be restored.

    Use [cmd]gage list --deleted[/] to list deleted runs that can be
    restored.
    """
    from .restore_impl import runs_restore, Args

    runs_restore(Args(ctx, runs or [], where, all, yes))
