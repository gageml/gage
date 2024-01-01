# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option

"""
TODO:

`--sort COLS` where COLS is a comma separated list of column keys or
label with optional leading '+/-' for ascending/descending order
respectively. Key should be whatever is used to join the values.

`--csv` to print results in a CSV table.

`--columns` can be used to limit and order the columns shown. This is a
convenience to limit/order columns with or without a board def.
"""

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
    json: JSONFlag = False,
):
    """Show a board of run results.

    [b]IMPORTANT:[/b] This feature is under development. It currently
    only supports showing a board as JSON. You must specify '--json' in
    this case.
    """
    from .board_impl import show_board, Args

    show_board(Args(runs or [], where, config, json))
