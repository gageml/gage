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

from .impl_support import archive_dir
from .impl_support import runs_table
from .impl_support import selected_runs

from .copy_impl import _copy_to_ as copy_runs


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
    return len(
        [
            name
            for name in safe_list_dir(dirname)
            if name.endswith(".meta") or name.endswith(".meta.zip")
        ]
    )


def _handle_delete(args: Args):
    assert args.delete
    dirname = find_archive_dir(args.delete)
    if not dirname:
        _no_such_archive_error(args.delete)
    runs = list_archive_runs(dirname)
    if runs:
        _user_confirm_delete(args, runs)
        _delete_runs(runs, "Deleting archive runs")
    delete_archive(dirname)
    cli.err(f"Deleted archive '{args.delete}'")
    _delete_empty_archive_dir(dirname)


def _no_such_archive_error(name: str) -> NoReturn:
    cli.exit_with_error(
        f"archive '{name}' does not exist.\n"
        "Use '[cmd]gage archive --list[/]' to show available names."
    )


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


def _delete_runs(runs: list[Run], description: str):
    for run in cli.track(
        runs,
        description=description,
        transient=True,
    ):
        delete_run(run)


def _delete_empty_archive_dir(dirname: str):
    if not os.listdir(dirname):
        os.rmdir(dirname)
        return True
    cli.err(
        f"NOTE: archive directory \"{os.path.relpath(dirname)}\" "
        "was not empty after deleting archived runs and was not deleted."
    )
    return False


def _handle_delete_empty(args: Args):
    empty = [archive for archive in iter_archives() if _run_count(archive.dirname) == 0]
    if not empty:
        cli.err("Nothing to delete")
        return
    _user_confirm_delete_empty(args, empty)
    deleted = 0
    for archive in empty:
        run_count = _run_count(archive.dirname)
        if run_count:
            cli.err(f"{archive.name} contains runs - skipping")
            continue
        delete_archive(archive.dirname)
        if _delete_empty_archive_dir(archive.dirname):
            deleted += 1
    if deleted:
        cli.err(f"Deleted {deleted} empty {'archive' if deleted == 1 else 'archives'}")
    else:
        cli.err("Nothing deleted")


def _user_confirm_delete_empty(args: Args, archives: list[ArchiveInfo]):
    if args.yes:
        return
    table = cli.Table("name", "runs")
    for name, dirname in archives:
        modified = archive_modified(dirname)
        table.add_row(
            cli.label(name),
            # Prompt assumes each archive is empty - rely on downstream
            # to handle non-empty cases
            "0",
        )
    cli.err(table)
    archives_count = (
        "1 empty archive" if len(archives) == 1 else f"{len(archives)} empty archives"
    )
    cli.err(f"You are about to delete {archives_count}.")
    if not cli.confirm(f"Continue?", default=True):
        raise SystemExit(0)


def _handle_rename(args: Args):
    cur_name, new_name = args.rename
    assert cur_name
    assert new_name
    set_archive_name(archive_dir(cur_name), new_name)


def _handle_archive(args: Args):

    runs = _selected_runs(args)
    _user_confirm_archive(args, runs)
    _archive_runs([run for i, run in runs], args)


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
    archive_dir, name = _ensure_archive_dir(args)
    copy_runs(runs, archive_dir, no_summary=True)
    touch_archive(archive_dir)
    _delete_runs(runs, "Removing local runs")
    cli.err(f"Moved {len(runs)} {'run' if len(runs) == 1 else 'runs'}")
    cli.out(
        f"{'Run' if len(runs) == 1 else 'Runs'} archived to '{name}'\n\n"
        f"Use '[cmd]gage restore --archive {shlex_quote(name)}[/]' to "
        "restore from this archive."
    )


def restore_runs(runs: list[Run], target_dir: str):
    """Restore runs to target dir.

    This is exposed for use by the restore command. This odd arrangement
    reflects the different approaches used to manage deleted runs and
    archived runs. This issue is under consideration for a refactor to
    bring some consistency to the topics.

    In particular, what's odd: `copy_runs` is behavior borrowed from the
    copy impl, `_delete_runs` is behavior only needed by archive, as
    archive is a copy+delete operation (whereas delete is a meta dir
    rename) - and this now exposed to restore, as restore handles both
    deleted and archived runs. Good times.
    """
    copy_runs(runs, target_dir, no_summary=True)
    _delete_runs(runs, "Removing archived runs")
    runs_count = "1 run" if len(runs) == 1 else f"{len(runs)} runs"
    cli.err(f"Restored {runs_count}")


def _ensure_archive_dir(args: Args):
    if args.name:
        assert not args.archive
        return (find_archive_dir(args.name) or make_archive_dir(args.name)), args.name
    elif args.archive:
        dirname = find_archive_dir(args.archive)
        if not dirname:
            _no_such_archive_error(args.archive)
        return dirname, args.archive
    else:
        name = _make_archive_name()
        return make_archive_dir(name), name


def _make_archive_name():
    return datetime.datetime.now().strftime("%Y-%m-%d-%H%M%S")
