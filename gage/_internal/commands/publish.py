# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option

Board = Annotated[
    str,
    Option(
        "--board",
        metavar="NAME",
        help="Board name to publish. Required if name is not specified in board config.",
    ),
]


RunArgs = Annotated[
    Optional[list[str]],
    Argument(
        metavar="[run]...",
        help=(
            "Runs to publish. [arg]run[/] may be a run ID, name, "
            "list index or slice. Default is to publish all runs."
        ),
        show_default=False,
    ),
]

Where = Annotated[
    str,
    Option(
        "-w",
        "--where",
        metavar="expr",
        help="Publish runs matching filter expression.",
    ),
]

Config = Annotated[
    str,
    Option(
        "-c",
        "--config",
        metavar="PATH",
        help="Use board configuration.",
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


def publish(
    board: Board = "",
    runs: RunArgs = None,
    where: Where = "",
    config: Config = "",
    yes: YesFlag = False,
):
    """Publish a board."""
    from .publish_impl import publish, Args

    publish(Args(board, runs or [], where, config, yes))
