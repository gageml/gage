# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

from .. import cli

from ..run_util import log_user_attrs

from .impl_support import runs_table
from .impl_support import selected_runs


class Args(NamedTuple):
    runs: list[str]
    set: str
    clear: bool
    where: str
    all: bool
    yes: bool


def label(args: Args):
    if not args.set and not args.clear:
        cli.exit_with_error(
            "Specify either '--set' or '--clear'\n\n"
            "Try '[cmd]gage label --help[/]' for help."
        )
    if not args.runs and not args.all:
        cli.exit_with_error(
            "Specify a run to modify or use '--all'.\n\n"
            "Use '[cmd]gage list[/]' to show available runs.\n\n"
            "Try '[cmd]gage label -h[/]' for additional help."
        )
    runs, from_count = selected_runs(args)
    if not runs:
        cli.exit_with_error("Nothing selected")
    _verify_label(args, runs)
    _apply_label(runs, args)
    cli.err(_applied_label_desc(runs, args))


def _verify_label(args: Args, runs: list[tuple[int, Run]]):
    if args.yes:
        return
    table = runs_table(runs)
    cli.out(table)
    run_count = "1 run" if len(runs) == 1 else f"{len(runs)} runs"
    if args.set:
        cli.err(f"You are about set the label of {run_count} to {args.set!r}.")
    elif args.clear:
        cli.err(f"You are about clear the label of {run_count}.")
    else:
        assert False
    cli.err()
    if not cli.confirm(f"Continue?"):
        raise SystemExit(0)


def _apply_label(runs: list[tuple[int, Run]], args: Args):
    attrs = {"label": args.set} if args.set else {}
    delete = ["label"] if args.clear else []
    for index, run in runs:
        log_user_attrs(run, attrs, delete)


def _applied_label_desc(runs: list[tuple[int, Run]], args: Args):
    assert args.set or args.clear
    action = "Set" if args.set else "Cleared"
    run_count = "1 run" if len(runs) == 1 else f"{len(runs)} runs"
    return f"{action} label for {run_count}"
