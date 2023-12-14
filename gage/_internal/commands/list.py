# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option

from .. import cli

RunArgs = Annotated[
    Optional[list[str]],
    Argument(
        metavar="[run]...",
        help=("Runs to list. [arg]run[/] may be a run ID, name, list index or slice."),
        show_default=False,
    ),
]

More = Annotated[
    Optional[list[bool]],
    Option(
        "-m",
        "--more",
        help="Show more runs.",
    ),
]

Limit = Annotated[
    int,
    Option(
        "-n",
        "--limit",
        metavar="max",
        help="Limit list to [b]max[/] runs.",
    ),
]

AllFlag = Annotated[
    bool,
    Option(
        "-a",
        "--all",
        help="Show all runs. Cannot use with --limit.",
        callback=cli.incompatible_with("limit"),
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

DeletedFlag = Annotated[
    bool,
    Option(
        "-d",
        "--deleted",
        help="Show deleted runs.",
    ),
]

SimplifiedFlag = Annotated[
    bool,
    Option(
        "-s",
        help="Simplified listing - used for tests",
        hidden=True,
    ),
]


def runs_list(
    runs: RunArgs = None,
    more: More = None,
    limit: Limit = 20,
    all: AllFlag = False,
    where: Where = "",
    deleted: DeletedFlag = False,
    simplified: SimplifiedFlag = False,
):
    """List runs.

    By default the latest 20 runs are shown. To show more runs, use '-m
    / --more' or '-n / --limit'. Use '-a / --all' to show all runs.

    Use '-w / --where' to filter runs. Try '[cmd]gage help filters[/]'
    for help with filter expressions.

    Runs may be selected from the list using run IDs, names, indexes or
    slice notation. Try '[cmd]gage help select-runs[/]' for help with
    select options.
    """
    from .list_impl import runs_list, Args

    runs_list(
        Args(
            runs or [],
            sum(more or []),
            limit,
            all,
            where,
            deleted,
            simplified,
        )
    )
