# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Typer
from typer import Context

from .. import cli

from ..cli import Argument
from ..cli import Option


RunSpecs = Annotated[
    Optional[list[str]],
    Argument(
        metavar="[run]...",
        help="Runs to select.",
        show_default=False,
    ),
]

Where = Annotated[
    str,
    Option(
        "-w",
        "--where",
        metavar="expr",
        help="Select runs matching filter expression.",
    ),
]

AllFlag = Annotated[
    bool,
    Option(
        "-a",
        "--all",
        help="Purge match files for all runs.",
    ),
]

FilePatterns = Annotated[
    Optional[list[str]],
    Option(
        "-f",
        "--file",
        metavar="pattern",
        help="File pattern using glob syntax.",
    ),
]

PreviewFlag = Annotated[
    bool,
    Option(
        "--preview",
        help="Show file select preview.",
    ),
]

ConfirmText = Annotated[
    str,
    Option(
        "--confirm",
        help="Purge without confirming by specifying 'I agree'.",
        hidden=True,
    ),
]


def purge_run_files(
    ctx: Context,
    runs: RunSpecs = None,
    where: Where = "",
    all: AllFlag = False,
    patterns: FilePatterns = None,
    preview: PreviewFlag = False,
    confirm_text: ConfirmText = "",
):
    """Permanently delete run files.

    [b yellow]WARNING:[/] This operation may break runs if it deletes
    critical files. Use when you're certain that the deleted files are
    no longer needed for the run. This is typically done to free disk
    space used by unintentionally generated files.
    """
    from .util_impl import purge_run_files, PurgeRunFilesArgs

    purge_run_files(
        PurgeRunFilesArgs(
            ctx,
            runs or [],
            where,
            all,
            patterns or [],
            preview,
            confirm_text,
        )
    )


def app():
    app = Typer(
        cls=cli.AliasGroup,
        help="Gage utility commands.",
        rich_markup_mode="rich",
        no_args_is_help=True,
        add_completion=False,
        pretty_exceptions_enable=not cli.is_plain,
        pretty_exceptions_show_locals=False,
        context_settings={
            "help_option_names": ("-h", "--help"),
        },
        subcommand_metavar="command",
        options_metavar="[options]",
    )
    app.command("purge-run-files")(purge_run_files)
    return app
