# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option

from .. import cli

RunSpecs = Annotated[
    Optional[list[str]],
    Argument(
        help="Runs to modify. Required unless '--all' is specified.",
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

Message = Annotated[
    str,
    Option(
        "-m",
        "--msg",
        metavar="msg",
        help=(
            "Message used for the comment. If not specified for a "
            "comment, the system editor opened."
        ),
        show_default=False,
    ),
]

ListFlag = Annotated[
    bool,
    Option(
        "-l",
        "--list",
        help="Show run comments.",
    ),
]

Delete = Annotated[
    str,
    Option(
        "-d",
        "--delete",
        metavar="comment",
        help="Delete [arg]comment[/]. Use '--list' to show comments.",
    ),
]

Edit = Annotated[
    str,
    Option(
        "-e",
        "--edit",
        metavar="comment",
        help="Edit [arg]comment[/]. Use '--list' show show comments.",
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


def comment(
    runs: RunSpecs = None,
    msg: Message = "",
    delete: Delete = "",
    edit: Edit = "",
    list: ListFlag = False,
    where: Where = "",
    all: AllFlag = False,
    yes: YesFlag = False,
):
    """Comment on runs.

    Uses the system editor by default to add a comment to one or more
    runs. Use '-m / --message' to specify the comment message as a
    command argument rather than use the system editor.

    Use '--list' to list run comments.

    Use '--delete' with a comment ID to delete a comment.

    Use '--edit' to edit a comment.
    """

    from .comment_impl import comment, Args

    comment(Args(runs or [], msg, delete, edit, list, where, all, yes))
