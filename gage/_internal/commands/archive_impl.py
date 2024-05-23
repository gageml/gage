# SPDX-License-Identifier: Apache-2.0

from typing import *

from gage._internal.file_util import safe_list_dir

from ..types import *

import datetime
import os

import human_readable

from .. import cli

from ..archive_util import *
from ..shlex_util import shlex_quote
from ..var import delete_run

from .impl_support import runs_table
from .impl_support import selected_runs

from .copy_impl import _copy_to_ as copy_runs


class Args(NamedTuple):
    runs: list[str]
    name: str
    list: bool
    rename: tuple[str, str]
    delete: str
    where: str
    all: bool
    yes: bool


def archive(args: Args):
    if args.list:
        _handle_list(args)
    elif args.delete:
        _handle_delete(args)
    elif args.rename[0] or args.rename[1]:
        _handle_rename(args)
    else:
        _handle_archive(args)


def _handle_list(args: Args):
    table = cli.Table("name", "runs", "last archived")
    for name, dirname in iter_archives():
        modified = archive_modified(dirname)
        table.add_row(
            cli.label(name),
            cli.value(str(_run_count(dirname))),
            cli.value(human_readable.date_time(modified) if modified else ""),
        )
    cli.out(table)


def _run_count(dirname: str):
    return len([name for name in safe_list_dir(dirname) if name.endswith(".meta")])


def _handle_delete(args: Args):
    assert args.delete
    dirname = find_archive_dir(args.delete)
    if not dirname:
        cli.exit_with_error(
            f"Archive '{args.delete}' does not exit.\n"
            "Use '[cmd]gage archive --list[/]' to show available names."
        )
    runs = list_archive_runs(dirname)
    _user_confirm_delete(args, runs)
    _delete_runs(runs)
    delete_archive(dirname)
    cli.err(f"Deleted archive '{args.delete}'")
    _delete_empty_archive_dir(dirname)


def _user_confirm_delete(args: Args, runs: list[Run]):
    if args.yes:
        return
    indexed_runs = [(i + 1, run) for i, run in enumerate(runs)]
    table = runs_table(indexed_runs)
    cli.err(table)
    run_count = "1 run" if len(runs) == 1 else f"{len(runs)} runs"
    cli.err(
        f"[red b]You are about to PERMANENTLY delete {run_count}. "
        "This cannot be undone.\n"
    )
    if not cli.confirm(f"Continue?", default=False):
        raise SystemExit(0)


def _delete_runs(runs: list[Run]):
    for run in cli.track(
        runs,
        description="Deleting archive runs",
        transient=True,
    ):
        delete_run(run, permanent=True)


def _delete_empty_archive_dir(dirname: str):
    if not os.listdir(dirname):
        os.rmdir(dirname)
        return
    cli.err(
        f"NOTE: archive directory \"{os.path.relpath(dirname)}\" "
        "was not empty after deleting archived runs and was not deleted."
    )


def _handle_rename(args: Args):
    pass


def _handle_archive(args: Args):
    runs = _selected_runs(args)
    _user_confirm_archive(args, runs)
    _archive_runs(runs, args)


def _selected_runs(args: Args):
    if not args.runs and not args.all:
        cli.exit_with_error(
            "Specify one or more runs to archive or use '--all'.\n\n"
            "Use '[cmd]gage list[/]' to show available runs.\n\n"
            "Try '[cmd]gage archive -h[/]' for additional help."
        )
    runs, from_count = selected_runs(args)
    if not runs:
        cli.exit_with_error("Nothing selected")
    return runs


def _user_confirm_archive(args: Args, runs: list[IndexedRun]):
    if args.yes:
        return
    table = runs_table(runs)
    cli.out(table)
    runs_count = "1 run" if len(runs) == 1 else f"{len(runs)} runs"
    cli.err(f"You are about to archive {runs_count}.")
    cli.err()
    if not cli.confirm(f"Continue?", default=True):
        raise SystemExit(0)


def _archive_runs(runs: list[IndexedRun], args: Args):
    archive_dir, name = _ensure_archive_dir(args)
    copy_runs([run for i, run in runs], archive_dir)
    touch_archive(archive_dir)
    runs_desc = "Run" if len(runs) == 1 else "Runs"
    cli.out(
        f"{runs_desc} archived to '{name}'\n\n"
        f"Use '[cmd]gage restore {shlex_quote(name)} --all[/]' to "
        "restore these runs later.\n"
        "Use '[cmd]gage archive --list[/]' to show available archives."
    )


def _ensure_archive_dir(args: Args):
    if args.name:
        return (find_archive_dir(args.name) or make_archive_dir(args.name)), args.name
    name = _make_archive_name()
    return make_archive_dir(name), name


def _make_archive_name():
    return datetime.datetime.now().strftime("%Y-%m-%d_%H:%M:%S")
