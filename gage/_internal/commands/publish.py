# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option


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


def publish(
    runs: RunArgs = None,
    where: Where = "",
    config: Config = "",
):
    """Publish a board."""
    from .publish_impl import publish, Args

    publish(Args(runs or [], where, config))
