# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option


RunArgs = Annotated[
    Optional[list[str]],
    Argument(
        metavar="[run]...",
        help=(
            "Runs to show. [arg]run[/] may be a run ID, name, "
            "list index or slice. Default is to show all runs."
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
        help="Show runs matching filter expression.",
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

CSVFlag = Annotated[
    bool,
    Option(
        "--csv",
        help="Show board as CSV output.",
    ),
]

JSONFlag = Annotated[
    bool,
    Option(
        "--json",
        help="Show board as JSON output.",
    ),
]


def show_board(
    runs: RunArgs = None,
    where: Where = "",
    config: Config = "",
    csv: CSVFlag = False,
    json: JSONFlag = False,
):
    """Show a board of run results.

    [b]IMPORTANT:[/b] This feature is under development. It currently
    only supports showing a board as CSV or JSON. You must specify
    either '--csv' or '--json'.
    """
    from .board_impl import show_board, Args

    show_board(Args(runs or [], where, config, csv, json))
