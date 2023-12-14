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
    permanent: bool
    yes: bool


def runs_delete(args: Args):
    if not args.runs and not args.all:
        cli.exit_with_error(
            "Specify a run to delete or use '--all'.\n\n"
            "Use '[cmd]gage list[/]' to show available runs.\n\n"
            f"Try '[cmd]{args.ctx.command_path} {args.ctx.help_option_names[0]}[/]' "
            "for additional help."
        )
    runs, from_count = selected_runs(args)
    if not runs:
        cli.exit_with_error("Nothing selected")
    _verify_delete(args, runs)
    deleted = var.delete_runs(_strip_index(runs), args.permanent)
    cli.err(_deleted_msg(deleted, args))


def _verify_delete(args: Args, runs: list[tuple[int, Run]]):
    if args.yes:
        return
    table = runs_table(runs)
    cli.out(table)
    permanent_prefix = "permanently " if args.permanent else ""
    run_count = "1 run" if len(runs) == 1 else f"{len(runs)} runs"
    cli.err(f"You are about to {permanent_prefix}delete {run_count}.")
    cli.err()
    if not cli.confirm(f"Continue?", default=not args.permanent):
        raise SystemExit(0)


def _strip_index(runs: list[tuple[int, Run]]):
    return [run[1] for run in runs]


def _deleted_msg(runs: list[Run], args: Args):
    if not runs:
        return "Nothing deleted"
    action = "Permanently deleted" if args.permanent else "Deleted"
    if len(runs) == 1:
        return f"{action} 1 run"
    return f"{action} {len(runs)} runs"
