# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import rich.box

from rich.console import Group
from rich.markup import render
from rich.padding import Padding
from rich.panel import Panel
from rich.table import Table

from . import cli
from . import typer_rich_util
from . import run_config_util

__all__ = [
    "get_help",
    "config_table",
]


def get_help(opspec: str, context: RunContext):
    usage = render(f"[b]Usage: gage run {opspec}")
    help = render((context.opdef.get_description() or "").strip())
    config = run_config_util.read_project_config(context.project_dir, context.opdef)
    return Group(
        Padding(
            Group(
                usage,
                Padding(help, (1 if help else 0, 0, 0, 0)),
            ),
            (0, 1),
        ),
        *([Padding(FlagsPanel(config), (1, 0, 0, 0))] if config else []),
    )


def FlagsPanel(config: dict[str, RunConfigValue]):
    flags_table = Table(
        highlight=True,
        show_header=True,
        expand=True,
        box=None,
    )
    flags_table.add_column("", style=typer_rich_util.STYLE_OPTION)
    flags_table.add_column("Default", header_style="dim i")
    for key, val in sorted(config.items()):
        flags_table.add_row(key, str(val))
    return Panel(
        flags_table,
        box=rich.box.MARKDOWN if cli.is_plain else rich.box.ROUNDED,
        border_style=typer_rich_util.STYLE_OPTIONS_PANEL_BORDER,
        title="Flags",
        title_align=typer_rich_util.ALIGN_OPTIONS_PANEL,
        width=_panel_width(),
    )


def _panel_width():
    # Workaround for inconsistency in panel width across platform.
    # It seems that reducing the width by 1 creates a consistent
    # width, whereas without this Windows suffers the 1-space less
    # wide problem.
    import os

    if "COLUMNS" in os.environ:
        return int(os.environ["COLUMNS"]) - 1
    return None


def config_table(config: RunConfig):
    table = Table.grid(padding=(0, 2))
    table.add_column(style=typer_rich_util.STYLE_OPTION)
    table.add_column(header_style="dim i")
    for key, val in sorted(config.items()):
        table.add_row(key, _format_config_val(val))
    return Padding(table, 1)


def _format_config_val(val: Any):
    if val is True:
        return "true"
    if val is False:
        return "false"
    return str(val)
