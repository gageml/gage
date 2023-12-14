# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option

from .. import cli


Path = Annotated[
    str,
    Argument(
        metavar="[path]",
        help=(
            "Gage file or a project directory to check. Cannot " "use with --version."
        ),
        callback=cli.incompatible_with("version"),
    ),
]

Version = Annotated[
    str,
    Option(
        "--version",
        metavar="spec",
        help=(
            "Test Gage version against [arg]spec[/]. Cannot "
            "be used with [arg]path[/]."
        ),
    ),
]

JSONFlag = Annotated[
    bool,
    Option(
        "--json",
        help="Format check output as JSON.",
    ),
]

VerboseFlag = Annotated[
    bool,
    Option(
        "-v",
        "--verbose",
        help="Show more information.",
    ),
]


def check(
    path: Path = "",
    version: Version = "",
    json: JSONFlag = False,
    verbose: VerboseFlag = False,
):
    """Show and validate settings.

    Shows Gage ML version, install location, and other configured
    settings.

    To check a Gage file for issues, specify [arg]path[/].
    """
    from .check_impl import check, Args

    check(Args(path, version, json, verbose))
