# SPDX-License-Identifier: Apache-2.0

from typing import *

import itertools
import os

from ..types import *

from .. import cli

from ..lang import parse_config_value

from .run_impl import Args
from .run_impl import RunContext
from .run_impl import _stage as _stage_run
from .run_impl import _exec_and_finalize


class BatchFileReadError(Exception):
    def __init__(self, msg: str):
        self.msg = msg

    def __str__(self):
        return self.msg


class BatchFile:

    def __iter__(self) -> Generator[RunConfig, None, None]: ...

    def __len__(self) -> int: ...

    def __str__(self) -> str: ...


class Batch:
    def __init__(self, batchfiles: list[BatchFile], max_runs: int = -1):
        self.batchfiles = batchfiles
        self._run_config = _trunc(_batch_run_config(batchfiles), max_runs)

    def __iter__(self):
        return iter(self._run_config)

    def __len__(self):
        return len(self._run_config)

    def __str__(self):
        return ", ".join([str(f) for f in self.batchfiles])


T = TypeVar("T")


def _trunc(l: list[T], n: int):
    return l if n < 0 else l[:n]


def _batch_run_config(batchfiles: list[BatchFile]):
    return [
        _merge_config_rows(config_rows)
        for config_rows in itertools.product(*batchfiles)
    ]


def _merge_config_rows(rows: tuple[dict[str, Any], ...]):
    return RunConfig([kv for config in rows for kv in config.items()])


class CsvBatchFile(BatchFile):
    def __init__(self, filename: str):
        self._rows = _read_csv_rows(filename)
        self._filename = filename

    def __len__(self):
        return len(self._rows)

    def __iter__(self):
        return iter(self._rows)

    def __str__(self):
        return self._filename


def _read_csv_rows(filename: str):
    import csv

    try:
        f = open(filename)
    except FileNotFoundError:
        raise BatchFileReadError(f"Batch file {filename} does not exist")
    else:
        with f:
            return [
                RunConfig(
                    {str(key): parse_config_value(val) for key, val in row.items()}
                )
                for row in csv.DictReader(f)
            ]


class JsonBatchFile(BatchFile):
    def __init__(self, filename: str):
        self._items = _read_json_items(filename)
        self._filename = filename

    def __len__(self):
        return len(self._items)

    def __iter__(self):
        return iter(self._items)

    def __str__(self):
        return self._filename


def _read_json_items(filename: str):
    import json

    try:
        f = open(filename)
    except FileNotFoundError:
        raise BatchFileReadError(f"Batch file {filename} does not exist")
    else:
        with f:
            try:
                data = json.load(f)
            except json.JSONDecodeError as e:
                raise BatchFileReadError(
                    f"Cannot read batch file {filename}: {e}"
                ) from None
        if not isinstance(data, list):
            cli.exit_with_error(f"Expected an array of objects in {filename}")
        return [RunConfig(row) for row in data]


def handle_run_context(context: RunContext, args: Args):
    try:
        batch = _init_batch(args)
    except BatchFileReadError as e:
        _handle_batchfile_encoding_error(e)
    else:
        _handle_batch(batch, context, args)


def _handle_batchfile_encoding_error(e: BatchFileReadError):
    cli.exit_with_error(str(e))


def _handle_batch(batch: Batch, context: RunContext, args: Args):
    if len(batch) == 0:
        cli.exit_with_error(f"Nothing to run in {batch}")
    if args.preview:
        _preview(batch, context, args)
    elif args.stage:
        _handle_stage(batch, context, args)
    else:
        _run(batch, context, args)


def _init_batch(args: Args):
    assert args.batch
    return Batch(
        [_init_batchfile(filename) for filename in args.batch],
        args.max_runs,
    )


def _init_batchfile(filename: str):
    ext = os.path.splitext(filename)[1].lower()
    if ext == ".csv":
        return CsvBatchFile(filename)
    elif ext == ".json":
        return JsonBatchFile(filename)
    else:
        cli.exit_with_error(f"Unsupported extension for batch file {filename}")


def _preview(batch: Batch, context: RunContext, args: Args):
    # TODO
    cli.exit_with_error("Batch preview is not yet implemented")


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
    runs: list[Run] = []
    for config in batch:
        runs.append(_stage_run(context, run_args, config))
    return runs


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
    # Set the following per-run args:
    #   --yes     User confirms the batch, not each run
    return Args(
        **{
            **args._asdict(),
            "yes": True,
        }
    )


def _run(batch: Batch, context: RunContext, args: Args):
    run_args = _run_args_for_batch(args)
    runs = _stage(batch, context, args)
    for run in runs:
        try:
            _exec_and_finalize(run, run_args)
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
