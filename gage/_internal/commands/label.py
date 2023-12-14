# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option

from ..cli import incompatible_with

RunSpecs = Annotated[
    Optional[list[str]],
    Argument(
        help="Runs to modify. Required unless '--all' is specified.",
        metavar="[run]...",
        show_default=False,
        callback=incompatible_with("all"),
    ),
]

Where = Annotated[
    str,
    Option(
        "-w",
        "--where",
        metavar="expr",
        help="Modify runs matching filter expression.",
    ),
]

AllFlag = Annotated[
    bool,
    Option(
        "-a",
        "--all",
        help="Modify all runs.",
    ),
]

SetLabel = Annotated[
    str,
    Option(
        "-s",
        "--set",
        metavar="label",
        help="Set label for the specified runs.",
        show_default=False,
        callback=incompatible_with("clear"),
    ),
]

ClearFlag = Annotated[
    bool,
    Option(
        "-c",
        "--clear",
        help="Clear the label for the specified runs.",
        callback=incompatible_with("set"),
    ),
]

YesFlag = Annotated[
    bool,
    Option(
        "-y",
        "--yes",
        help="Make changes without prompting.",
    ),
]


def label(
    runs: RunSpecs = None,
    set: SetLabel = "",
    clear: ClearFlag = False,
    where: Where = "",
    all: AllFlag = False,
    yes: YesFlag = False,
):
    """Set or clear run labels.

    Either '--set' or '--clear' is required.
    """

    from .label_impl import label as set_label, Args

    set_label(Args(runs or [], set, clear, where, all, yes))
