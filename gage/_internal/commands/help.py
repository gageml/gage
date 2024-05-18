# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Typer

import os

from ...__init__ import __pkgdir__

from .. import cli


def help():
    """Show help for a topic."""


def filters():
    """Filter results."""
    _show_help("filters")


def operations():
    """Define and run operations."""
    _show_help("operations")


def gagefile():
    """Define and use gage files."""
    _show_help("gagefile")


def exec():
    """Define run commands."""
    _show_help("exec")


def remotes():
    """Define and use remote locations."""
    _show_help("remotes")


def batches():
    """Define and run batches."""
    _show_help("batches")


def _show_help(topic: str):
    help = _read_help_topic(topic)
    with cli.pager():
        if cli.is_plain:
            cli.out(help)
        else:
            import rich.markdown

            md = rich.markdown.Markdown(help)
            cli.out(md, wrap=True)


def _read_help_topic(topic: str):
    filename = os.path.join(__pkgdir__, "gage", "help", f"{topic}.md")
    with open(filename) as f:
        return f.read()


def help_app():
    app = Typer(
        rich_markup_mode="rich",
        no_args_is_help=True,
        add_completion=False,
        subcommand_metavar="topic",
        options_metavar="[options]",
    )
    app.callback()(help)

    def topic(fn: Callable[..., Any]):
        app.command(rich_help_panel="Topics")(fn)

    topic(batches)
    topic(exec)
    topic(filters)
    topic(gagefile)
    topic(operations)
    topic(remotes)

    return app
