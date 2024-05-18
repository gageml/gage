# SPDX-License-Identifier: Apache-2.0

from typing import *

import os
import sys

from ..types import *

from .. import cli

from ..lang import parse_config_value

from .run_impl import Args
from .run_impl import RunContext
from .run_impl import _stage as _stage_run
from .run_impl import _exec_and_finalize


class Batch:

    def __iter__(self) -> Generator[RunConfig, None, None]: ...

    def __len__(self) -> int: ...

    def __str__(self) -> str: ...


class StdinBatch(Batch):
    def __init__(self, max_runs: int):
        self._max_runs = max_runs

    def __len__(self):
        return -1

    def __iter__(self):
        n = 0
        while True:
            if self._max_runs >= 0 and n == self._max_runs:
                break
            line = sys.stdin.readline()
            if not line:
                break
            yield _parse_stdin_line(line)
            n += 1

    def __str__(self):
        return "standard input"


def _parse_stdin_line(line: str):
    assert False, line


class CsvBatch(Batch):
    def __init__(self, filename: str, max_runs: int):
        self._rows = _trunc(_read_csv_rows(filename), max_runs)
        self._filename = filename

    def __len__(self):
        return len(self._rows)

    def __iter__(self):
        return iter(self._rows)

    def __str__(self):
        return self._filename


def _trunc(l: list[Any], n: int):
    return l if n < 0 else l[:n]


def _read_csv_rows(filename: str):
    import csv

    with open(filename) as f:
        return [
            {key: parse_config_value(val) for key, val in row.items()}
            for row in csv.DictReader(f)
        ]


class JSONBatch(Batch):
    def __init__(self, filename: str, max_runs: int):
        self._items = _trunc(_read_json_items(filename), max_runs)
        self._filename = filename

    def __len__(self):
        return len(self._items)

    def __iter__(self):
        return iter(self._items)

    def __str__(self):
        return self._filename


def _read_json_items(filename: str) -> list[Any]:
    import json

    with open(filename) as f:
        data = json.load(f)
    if not isinstance(data, list):
        cli.exit_with_error(f"Expected an array of objects in {filename}")
    return data


def handle_run_context(context: RunContext, args: Args):
    batch = _init_batch(args)
    if len(batch) == 0:
        _handle_empty_batch(batch)
    elif args.preview:
        _preview(batch, context, args)
    elif args.stage:
        _handle_stage(batch, context, args)
    else:
        _run(batch, context, args)


def _handle_empty_batch(batch: Batch):
    cli.exit_with_error(f"Nothing to run in {batch}")


def _init_batch(args: Args):
    assert args.batch
    batchfile = args.batch
    if batchfile == "-":
        return StdinBatch(args.max_runs)
    ext = os.path.splitext(batchfile)[1].lower()
    if ext == ".csv":
        return CsvBatch(batchfile, args.max_runs)
    elif ext == ".json":
        return JSONBatch(batchfile, args.max_runs)
    else:
        cli.exit_with_error(f"Unsupported extension for batch file {batchfile}")


def _preview(batch: Batch, context: RunContext, args: Args):
    assert False


def _handle_stage(batch: Batch, context: RunContext, args: Args):
    runs = _stage(batch, context, args)
    cli.out(
        f"Staged {len(runs)} {'run' if len(runs) == 1 else 'runs'}\n\n"
        "To list staged runs, use '[cmd]gage runs --where staged[/]'\n"
        "To start a run, use '[cmd]gage run --start <run>[/]'"
    )


def _stage(batch: Batch, context: RunContext, args: Args):
    _verify_run_or_stage(args, batch, context)
    run_args = _run_args_for_batch(args)
    return [_stage_run(context, run_args, config) for config in batch]


def _verify_run_or_stage(args: Args, batch: Batch, context: RunContext):
    if args.yes:
        return
    cli.out(_action_desc(args, batch, context), err=True)
    if not cli.confirm(f"Continue?"):
        raise SystemExit(0)


def _action_desc(args: Args, batch: Batch, context: RunContext):
    action = "stage" if args.stage else "run"
    return (
        f"You are about to {action} [yellow]{context.opref.op_name}[/] "
        f"in a batch of {len(batch)} (defined in [cyan]{batch}[/])"
    )


def _run_args_for_batch(args: Args):
    # Ensure that 'yes' is True for runs - user has already verified
    return Args(**{**args._asdict(), "yes": True})


def _run(batch: Batch, context: RunContext, args: Args):
    runs = _stage(batch, context, args)
    for run in runs:
        try:
            _exec_and_finalize(run, args)
        except SystemExit as e:
            if e.code != 0:
                _print_run_error(run, e.code)
                raise


def _print_run_error(run: Run, code: int | str | None):
    cli.out(
        (
            f"\n[red b]Run {run.name} exited with an error ({code})[/]\n"
            f"Try '[cmd]gage show {run.name}[/]' for run details."
        ),
        err=True,
    )
