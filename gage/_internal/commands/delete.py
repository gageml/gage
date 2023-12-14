# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import *

import click

from .. import cli


RunSpecs = Annotated[
    Optional[list[str]],
    Argument(
        help="Runs to delete. Required unless '--all' is specified.",
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
        help="Delete runs matching filter expression.",
    ),
]

AllFlag = Annotated[
    bool,
    Option(
        "-a",
        "--all",
        help="Delete all runs.",
    ),
]

PermanentFlag = Annotated[
    bool,
    Option(
        "-p",
        "--permanent",
        help="Permanently delete runs. By default deleted runs can be restored.",
    ),
]

YesFlag = Annotated[
    bool,
    Option(
        "-y",
        "--yes",
        help="Delete runs without prompting.",
    ),
]


def runs_delete(
    ctx: click.Context,
    runs: RunSpecs = None,
    where: Where = "",
    all: AllFlag = False,
    permanent: PermanentFlag = False,
    yes: YesFlag = False,
):
    """Delete runs.

    [arg]run[/] is either a run index, a run ID, or a run name. Partial
    values may be specified for run ID and run name if they uniquely
    identify a run. Multiple runs may be specified.
    """
    from .delete_impl import runs_delete, Args

    runs_delete(Args(ctx, runs or [], where, all, permanent, yes))
