# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import *

import click

from .. import cli


RunSpecs = Annotated[
    Optional[list[str]],
    Argument(
        help="Runs to remove. Required unless '--all' is specified.",
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
        help="Remove runs matching filter expression.",
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
        help="Remove runs without prompting.",
    ),
]


def runs_purge(
    ctx: click.Context,
    runs: RunSpecs = None,
    where: Where = "",
    all: AllFlag = False,
    yes: YesFlag = False,
):
    """Remove deleted runs.

    Use to remove deleted runs, freeing disk space. Note that purged
    runs cannot be recovered.

    Use [cmd]gage list --deleted[/] to list deleted runs that can be
    removed.
    """
    from .purge_impl import runs_purge, Args

    runs_purge(Args(ctx, runs or [], where, all, yes))
