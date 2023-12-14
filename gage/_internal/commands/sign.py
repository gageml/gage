# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument

RunSpecs = Annotated[
    Optional[list[str]],
    Argument(
        metavar="[run]...",
        help="Runs to select.",
        show_default=False,
    ),
]


def sign(runs: RunSpecs):
    """Sign one or more runs."""
    print("TODO: sign some runs")
