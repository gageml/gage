# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import datetime

import human_readable

from .. import cli
from .. import var

from ..shlex_util import shlex_quote

from ..run_archive import *

from .impl_support import archive_for_name as archive_for_name_or_exit
from .impl_support import runs_table
from .impl_support import selected_runs


class Args(NamedTuple):
    runs: list[str]
    name: str
    archive: str
    list: bool
    rename: tuple[str, str]
    delete: str
    delete_empty: bool
    where: str
    all: bool
    yes: bool


def archive(args: Args):
    if args.list:
        _handle_list(args)
    elif args.delete:
        _handle_delete(args)
    elif args.delete_empty:
        _handle_delete_empty(args)
    elif args.rename[0] or args.rename[1]:
        _handle_rename(args)
    else:
        _handle_archive(args)


def _handle_list(args: Args):
    table = cli.Table()
    table.add_column("name")
    table.add_column("runs", justify="right")
    table.add_column("last archived")
    for archive in iter_archives():
        run_count = _run_count(archive)
        table.add_row(
            cli.label(archive.get_name()),
            cli.value(str(run_count)),
            cli.value(_formatted_last_archived(archive)) if run_count > 0 else "",
        )
    cli.out(table)


def _run_count(archive: ArchiveDef):
    return len(var.list_runs(container=archive.get_id()))


def _formatted_last_archived(archive: ArchiveDef):
    last_archived = archive.get_last_archived()
    if not last_archived:
        return ""
    return human_readable.date_time(datetime.datetime.fromtimestamp(last_archived))


def _handle_delete(args: Args):
    assert args.delete
    archive = archive_for_name_or_exit(args.delete)
    runs = var.list_runs(container=archive.get_id())
    if runs:
        cli.exit_with_error(
            f"archive [arg]{args.delete}[/arg] is not empty\n\n"
            "Remove the runs from the archive first by restoring or purging them.\n"
            "Try '[cmd]gage archive --help[/cmd]' for more information."
        )
    delete_archive(archive.get_id())
    cli.err(f"Deleted archive [arg]{args.delete}[/arg]")


def _handle_delete_empty(args: Args):
    empty = [archive for archive in iter_archives() if _run_count(archive) == 0]
    if not empty:
        cli.err("Nothing to delete")
        return
    _user_confirm_delete_empty(args, empty)
    deleted = 0
    for archive in empty:
        run_count = _run_count(archive)
        if run_count:
            cli.err(f"archive [arg]{archive.get_name()}[/arg] contains runs - skipping")
            continue
        delete_archive(archive.get_id())
        deleted += 1
    if deleted:
        cli.err(f"Deleted {deleted} empty {'archive' if deleted == 1 else 'archives'}")
    else:
        cli.err("Nothing deleted")


def _user_confirm_delete_empty(args: Args, archives: list[ArchiveDef]):
    if args.yes:
        return
    table = cli.Table("name", "runs")
    for archive in archives:
        table.add_row(
            cli.label(archive.get_name()),
            "0",
        )
    cli.err(table)
    archives_count = (
        "1 empty archive" if len(archives) == 1 else f"{len(archives)} empty archives"
    )
    cli.err(f"You are about to delete {archives_count}.")
    cli.err()
    if not cli.confirm(f"Continue?", default=True):
        raise SystemExit(0)


def _handle_rename(args: Args):
    cur_name, new_name = args.rename
    assert cur_name
    assert new_name
    archive = archive_for_name_or_exit(cur_name)
    update_archive(archive, name=new_name)


def _handle_archive(args: Args):
    _check_archive_arg(args)
    runs = _selected_runs(args)
    _user_confirm_archive(args, runs)
    _archive_runs([run for i, run in runs], args)


def _check_archive_arg(args: Args):
    if args.archive:
        archive_for_name_or_exit(args.archive)


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
    to_archive = f" to [{cli.STYLE_VALUE}]{args.name}[/]" if args.name else ""
    cli.err(f"You are about to archive {runs_count}{to_archive}.")
    cli.err()
    if not cli.confirm(f"Continue?", default=True):
        raise SystemExit(0)


def _archive_runs(runs: list[Run], args: Args):
    assert runs
    archive = _ensure_archive(args)
    var.move_runs(runs, archive.get_id())
    name = archive.get_name()
    cli.err(
        f"Archived {len(runs)} {'run' if len(runs) == 1 else 'runs'} "
        f"to [arg]{name}[/arg]\n\n"
        f"Use '[cmd]gage restore --archive {shlex_quote(name)}[/]' to "
        "restore from this archive."
    )


def _ensure_archive(args: Args):
    if args.name:
        try:
            return archive_for_name(args.name)
        except ArchiveNotFoundError:
            return make_archive(name=args.name)
    elif args.archive:
        archive = archive_for_name_or_exit(args.archive)
        update_archive(archive, last_archived=now_timestamp())
        return archive
    else:
        name = _make_archive_name()
        return make_archive(name)


def _make_archive_name():
    return datetime.datetime.now().strftime("%Y-%m-%d-%H%M%S")
