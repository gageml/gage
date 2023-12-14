# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Typer

import os

from ...__init__ import __pkgdir__

from .. import cli


def help():
    """Show help for a topic."""


def filters():
    """Filtering results."""
    _show_help("filters")


def operations():
    """Defining and running operations."""
    _show_help("operations")


def gagefile():
    """Defining and using gage files."""
    _show_help("gagefile")


def exec():
    """Defining run commands."""
    _show_help("exec")


def remotes():
    """Defining and using remote locations."""
    _show_help("remotes")


def _show_help(topic: str):
    help = _read_help_topic(topic)
    with cli.pager():
        cli.out(cli.markdown(help), wrap=True)


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

    topic(exec)
    topic(filters)
    topic(gagefile)
    topic(operations)
    topic(remotes)

    return app
