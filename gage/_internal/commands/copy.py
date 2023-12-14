# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import *

from .. import cli


RunSpecs = Annotated[
    Optional[list[str]],
    Argument(
        metavar="[run]...",
        help="Runs to copy. Required unless '--all' is specified.",
        show_default=False,
        callback=cli.incompatible_with("all"),
    ),
]

Dest = Annotated[
    str,
    Option(
        "-d",
        "--dest",
        "--to",
        metavar="path",
        help="Destination of copied runs.",
    ),
]

Src = Annotated[
    str,
    Option(
        "-s",
        "--src",
        "--from",
        metavar="path",
        help="Destination of copied runs.",
    ),
]

Where = Annotated[
    str,
    Option(
        "-w",
        "--where",
        metavar="expr",
        help="Copy runs matching filter expression.",
    ),
]

AllFlag = Annotated[
    bool,
    Option(
        "-a",
        "--all",
        help="Copy all runs.",
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

Verbose = Annotated[
    Optional[list[bool]],
    Option(
        "-v",
        "--verbose",
        help="Show copy details.",
    ),
]


def copy(
    runs: RunSpecs = None,
    dest: Dest = "",
    src: Src = "",
    where: Where = "",
    all: AllFlag = False,
    yes: YesFlag = False,
    verbose: Verbose = None,
):
    """Copy runs.

    Either '-s / --source' or '-d / --dest' is required. [arg]path[/]
    may be a directory of a remote path. Use '[cmd]gage help remotes[/]'
    for help with remote paths.

    [arg]run[/] is either a run index, a run ID, or a run name. Partial
    values may be specified for run ID and run name if they uniquely
    identify a run. Multiple runs may be specified.

    Use '--all' to copy all runs.
    """
    from .copy_impl import copy, Args

    copy(
        Args(
            runs or [],
            dest,
            src,
            where,
            all,
            yes,
            sum(verbose or []),
        )
    )
