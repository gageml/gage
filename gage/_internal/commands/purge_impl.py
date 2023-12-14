# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import click

from .. import cli
from .. import var

from .impl_support import runs_table
from .impl_support import selected_runs


class Args(NamedTuple):
    ctx: click.Context
    runs: list[str]
    where: str
    all: bool
    yes: bool


def runs_purge(args: Args):
    if not args.runs and not args.all:
        cli.exit_with_error(
            "Specify a deleted run to remove or use '--all'.\n\n"
            "Use '[cmd]gage list --deleted[/]' to show deleted runs.\n\n"
            f"Try '[cmd]{args.ctx.command_path} {args.ctx.help_option_names[0]}[/]' "
            "for additional help."
        )
    runs, from_count = selected_runs(args, deleted=True)
    if not runs:
        cli.exit_with_message("Nothing selected")
    _verify_purge(args, runs)
    removed = var.purge_runs(_strip_index(runs))
    cli.err(_removed_msg(removed, args))


def _verify_purge(args: Args, runs: list[tuple[int, Run]]):
    if args.yes:
        return
    table = runs_table(runs, deleted=True)
    cli.out(table)
    run_count = "1 deleted run" if len(runs) == 1 else f"{len(runs)} deleted runs"
    cli.err(f"You are about to permanently remove {run_count}.")
    if not cli.confirm(f"Continue?"):
        raise SystemExit(0)


def _strip_index(runs: list[tuple[int, Run]]):
    return [run[1] for run in runs]


def _removed_msg(runs: list[Run], args: Args):
    if not runs:
        return "Nothing removed"
    if len(runs) == 1:
        return f"Permanently removed 1 run"
    return f"Permanently removed {len(runs)} runs"
