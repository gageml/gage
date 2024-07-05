# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import fnmatch
import os

from typer import Context

from .. import cli

from ..file_util import delete_file
from ..file_util import format_file_size
from ..file_util import safe_ls

from .impl_support import selected_runs


class PurgeRunFilesArgs(NamedTuple):
    ctx: Context
    runs: list[str]
    where: str
    all: bool
    patterns: list[str]
    preview: bool
    confirm_text: str


def purge_run_files(args: PurgeRunFilesArgs):
    if not args.runs and not args.all:
        cli.exit_with_error(
            "Specify a run or use '--all'.\n\n"
            f"Try '[cmd]{args.ctx.command_path} {args.ctx.help_option_names[0]}[/]' "
            "for additional help."
        )
    if not args.patterns:
        cli.exit_with_error("Specify at least one file pattern using '-f / --files'")
    runs, _ = selected_runs(args)
    if not runs:
        cli.exit_with_message("Nothing selected")
    match_results = _apply_patterns(runs, args)
    if args.preview:
        _show_match_results_and_exit(match_results)
    matches = [
        (run, path) for run, path, matching_pattern in match_results if matching_pattern
    ]
    if not matches:
        cli.exit_with_error(
            "No files matched the specified patterns\n"
            "Use '--preview' to show pattern match details."
        )
    _verify_purge_run_files(args, matches)
    _purge_run_files(matches)


def _apply_patterns(
    runs: list[IndexedRun], args: PurgeRunFilesArgs
) -> list[tuple[Run, str, str | None]]:
    matched = []
    all_files = [(run[1], path) for run in runs for path in safe_ls(run[1].run_dir)]
    for run, path in cli.track(all_files, "Finding files", transient=True):
        for pattern in args.patterns:
            if fnmatch.fnmatch(path, pattern):
                matched.append((run, path, pattern))
                break
        else:
            matched.append((run, path, None))
    return matched


def _show_match_results_and_exit(match_results: list[tuple[Run, str, str | None]]):
    t = cli.Table("Run", "Path", "Matching Pattern")
    for run, path, matching_pattern in match_results:
        t.add_row(
            (
                f"[{cli.STYLE_SECOND_LABEL}]{run.name}"
                if matching_pattern
                else f"[{cli.STYLE_SUBTEXT}]{run.name}"
            ),
            (
                f"[{cli.STYLE_LABEL}]{path}"
                if matching_pattern
                else f"[{cli.STYLE_SUBTEXT}]{path}"
            ),
            (
                f"[{cli.STYLE_VALUE}]{matching_pattern}"
                if matching_pattern
                else f"[{cli.STYLE_SUBTEXT}]<no match>"
            ),
        )
    cli.err(t)
    raise SystemExit(0)


def _verify_purge_run_files(args: PurgeRunFilesArgs, matches: list[tuple[Run, str]]):
    if args.confirm_text == "I agree":
        return
    t = cli.Table()
    t.add_column("Run", style=cli.STYLE_SECOND_LABEL)
    t.add_column("Path", style=cli.STYLE_LABEL)
    t.add_column("Size", style="magenta")
    for run, path in matches:
        stat = os.stat(os.path.join(run.run_dir, path))
        t.add_row(run.name, path, format_file_size(stat.st_size))
    cli.err(t)
    if not cli.confirm(f"Continue?", default=False):
        raise SystemExit(0)


def _purge_run_files(matches: list[tuple[Run, str]]):
    for run, path in cli.track(matches, "Purging run files", transient=True):
        filename = os.path.join(run.run_dir, path)
        delete_file(filename)
    cli.err(f"Deleted {len(matches)} file(s)")
