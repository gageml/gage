# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import click

from .. import cli
from .. import var

from .impl_support import archive_for_name
from .impl_support import runs_table
from .impl_support import selected_runs


class Args(NamedTuple):
    ctx: click.Context
    runs: list[str]
    where: str
    archive: str
    all: bool
    yes: bool


def runs_restore(args: Args):
    args = _apply_implicit_all_for_archive(args)
    if not args.runs and not args.all:
        assert not args.archive
        cli.exit_with_error(
            "Specify a run to restore or use '--all'.\n\n"
            f"Use '[cmd]gage list --deleted[/]' to show deleted runs.\n\n"
            f"Try '[cmd]{args.ctx.command_path} {args.ctx.help_option_names[0]}[/]' "
            "for additional help."
        )
    runs, from_count = selected_runs(
        args,
        deleted=not args.archive,
        archive=archive_for_name(args.archive).get_id() if args.archive else None,
    )
    if not runs:
        cli.exit_with_message("Nothing selected")
    _verify_restore(args, runs)
    to_restore = _strip_index(runs)
    var.restore_runs(to_restore)
    cli.err(_restored_msg(to_restore, args))


def _apply_implicit_all_for_archive(args: Args):
    if not args.runs and args.archive:
        return Args(**{**args._asdict(), "all": True})
    return args


def _verify_restore(args: Args, runs: list[tuple[int, Run]]):
    if args.yes:
        return
    table = runs_table(runs, deleted=True)
    cli.out(table)
    run_count = "1 run" if len(runs) == 1 else f"{len(runs)} runs"
    cli.err(f"You are about to restore {run_count}.")
    cli.err()
    if not cli.confirm(f"Continue?"):
        raise SystemExit(0)


def _strip_index(runs: list[tuple[int, Run]]):
    return [run[1] for run in runs]


def _restored_msg(runs: list[Run], args: Args):
    if not runs:
        return "Nothing restored"
    if len(runs) == 1:
        return f"Restored 1 run"
    return f"Restored {len(runs)} runs"
