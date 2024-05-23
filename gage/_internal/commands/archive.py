# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..cli import Argument
from ..cli import Option

RunSpecs = Annotated[
    Optional[list[str]],
    Argument(
        help="Runs to archive. Required unless '--all' is specified.",
        metavar="[run]...",
        show_default=False,
        incompatible_with=["all"],
    ),
]

Where = Annotated[
    str,
    Option(
        "-w",
        "--where",
        metavar="expr",
        help="Archive runs matching filter expression.",
    ),
]

AllFlag = Annotated[
    bool,
    Option(
        "-a",
        "--all",
        help="Archive all runs.",
    ),
]

Name = Annotated[
    str,
    Option(
        "-n",
        "--name",
        metavar="name",
        help="Use the specified archive name.",
        show_default=False,
    ),
]

ListFlag = Annotated[
    bool,
    Option(
        "-l",
        "--list",
        help="Show local archives.",
        incompatible_with=["name", "delete", "rename"],
    ),
]

Rename = Annotated[
    tuple[str, str],
    Option(
        "-r",
        "--rename",
        metavar="current new",
        help=(
            "Rename the archive named [arg]current[/] to [arg]new[/]. "
            "Use '--list' show show comments."
        ),
    ),
]

Delete = Annotated[
    str,
    Option(
        "-d",
        "--delete",
        metavar="name",
        help="Delete the specified archive. Use '--list' to show archives.",
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


def archive(
    runs: RunSpecs = None,
    name: Name = "",
    delete: Delete = "",
    rename: Rename = ("", ""),
    list: ListFlag = False,
    where: Where = "",
    all: AllFlag = False,
    yes: YesFlag = False,
):
    """Archive runs.

    By default, selected runs are moved to a new archive with a default
    name. Use '--name' to specify a different name. If an archive with a
    specified name exists, selected runs are added to that archive.

    Use '--list' to list archives.

    Use '--delete' to delete an archive. WARNING: Deleted archives
    cannot be recovered.

    Use '--rename' to rename an archive.

    To restore runs from an archive, use 'gage restore --archive
    <name>'. To view runs in an archive use 'gage runs --archive
    <name>'.
    """

    from .archive_impl import archive, Args

    archive(
        Args(
            runs or [],
            name,
            list,
            rename,
            delete,
            where,
            all,
            yes,
        )
    )
