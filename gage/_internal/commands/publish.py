# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..cli import Argument
from ..cli import Option

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

Board = Annotated[
    str,
    Option(
        "--board-id",
        metavar="ID",
        help=(
            "Board ID to publish. Required if 'id' attribute is not "
            "specified in board config."
        ),
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
        help=(
            "Use board configuration. Defaults to board.json, "
            "board.yaml, or board.yaml if present."
        ),
    ),
]

NoConfig = Annotated[bool, Option("--no-config", help="Don't use config.")]

SkipRunsFlag = Annotated[
    bool,
    Option(
        "--skip-runs",
        help="Don't copy runs.",
    ),
]

YesFlag = Annotated[
    bool,
    Option(
        "-y",
        "--yes",
        help="Publish without prompting.",
    ),
]


def publish(
    board: Board = "",
    runs: RunArgs = None,
    where: Where = "",
    config: Config = "",
    no_config: NoConfig = False,
    skip_runs: SkipRunsFlag = False,
    yes: YesFlag = False,
):
    """Publish a board."""
    from .publish_impl import publish, Args

    publish(Args(board, runs or [], where, config, no_config, skip_runs, yes))
