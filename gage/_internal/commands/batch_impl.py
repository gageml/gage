# SPDX-License-Identifier: Apache-2.0

from typing import *

import itertools
import logging
import os

import rich.progress
import rich.status
import rich.table
import rich.live

from ..types import *

from .. import cli

from ..lang import parse_config_value

from ..run_util import run_phase_channel
from ..run_util import meta_opref

from .run_impl import _RUN_PHASE_DESC
from .run_impl import Args
from .run_impl import RunContext
from .run_impl import _RunPhaseContext
from .run_impl import _stage as _stage_run
from .run_impl import _exec_and_finalize


log = logging.getLogger(__name__)


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


def _BatchStatus(batch: Batch | list[Run], args: Args, staging: bool = False):
    if args.quiet:
        return _NullStatus()
    return _BatchProgress(len(batch), staging)


class _BatchProgress(_RunPhaseContext):
    """Batch progress facility.

    Batch progress is handled independently by this class.

    There are two components to batch progress: an overall batch
    progress bar and a run status line. This facility does not support
    per-run progress bars in the way that run_impl does. This is an area
    for future enhancement. Refer to run_impl for the pattern used to
    upgrade status (which is used as the indeterminate state prior to
    progress reporting) to a progress bar. The implementation is
    complicated here as we're using rich Live with a table

    Batch progress supports two modes: staging and running. When
    staging, overall progress shows a generic "Staging runs" label. When
    running, overall progress shows the current run one-based counter.

    All run output is send to standard output. Progress and status are
    transient.

    It should be noted that this class uses an unorthodox pattern for
    context managers that works at two levels: the batch context and run
    contexts.

        batch_progress = _BatchProgress()
        with batch_progress:
          # Batch started, no runs started - this is level 1
          for run in runs:
            with batch_progress:
              # Run started - this is level 2
              do_something(run)
            # Run stopped
        # Batch stopped

    This interface reflects the unified nature of the batch progress,
    which supports an overall status and a per-run status.
    """

    def __init__(self, run_count: int, staging: bool):
        self._run_count = run_count
        self._staging = staging
        self._cur_run = 0
        self._live_started = False
        self._run_started = False
        self._batch_progress = rich.progress.Progress()
        self._run_status = rich.status.Status("")
        progress_table = rich.table.Table.grid()
        progress_table.add_row(self._batch_progress)
        progress_table.add_row(self._run_status)
        self._batch_task = self._batch_progress.add_task(
            "[dim]Staging runs" if staging else "",
            total=run_count,
        )
        self._live = rich.live.Live(progress_table, transient=True)

    def __enter__(self):
        if not self._live_started:
            self._start_live()
            self._live_started = True
        else:
            assert not self._run_started
            self._handle_run_start()
            self._run_started = True
        return self

    def _start_live(self):
        self._live.start()

    def _handle_run_start(self):
        self._run_status.update("")
        self._cur_run += 1
        self._batch_progress.update(self._batch_task, advance=1)
        if not self._staging:
            self._batch_progress.update(
                self._batch_task,
                description=f"[dim]Batch run {self._cur_run} of {self._run_count}",
            )
        run_phase_channel.add(self)

    def __exit__(self, *exc: Any):
        assert self._live_started
        if self._run_started:
            self._handle_run_stop()
            self._run_started = False
        else:
            self._stop_live()
            self._live_started = False

    def _handle_run_stop(self):
        run_phase_channel.remove(self)
        self._run_status.update("")

    def _stop_live(self):
        self._live.stop()

    def __call__(self, name: str, arg: Any):
        if name == "exec-output":
            self._handle_run_output(arg)
        else:
            self._handle_run_status(name, arg)

    def _handle_run_output(self, arg: Any):
        run, phase_name, stream, output, progress = arg
        self._live.console.out(output.decode(), end="")

    def _handle_run_status(self, name: str, arg: Any):
        desc = _run_status_desc(name, arg)
        if desc is not None:
            self._run_status.update(desc)


def _run_status_desc(name: str, arg: Any):
    desc = _RUN_PHASE_DESC.get(name)
    if desc is None:
        return None
    try:
        return desc.format(**_run_attrs_for_phase_arg(arg))
    except KeyError as e:
        log.debug(
            "Run phase desc format error for %r: missing %r",
            desc,
            e.args[0],
        )
        return desc


def _run_attrs_for_phase_arg(arg: Any) -> dict[str, Any]:
    run = _run_for_phase_arg(arg)
    return {"op_name": meta_opref(run).op_name} if run else {}


def _run_for_phase_arg(arg: Any):
    if isinstance(arg, Run):
        return arg
    elif arg and isinstance(arg, tuple) and isinstance(arg[0], Run):
        return arg[0]
    else:
        return None


class _NullStatus(_RunPhaseContext):
    """Batch status that doesn't show anything.

    Used for batch status when the quiet option is specified.
    """

    def __enter__(self):
        return self

    def __exit__(self, *exc: Any):
        pass


def _stage(batch: Batch, context: RunContext, args: Args):
    _verify_run_or_stage(args, batch, context)
    run_args = _run_args_for_batch(args)
    runs: list[Run] = []
    with _BatchStatus(batch, args, staging=True) as status:
        for config in batch:
            runs.append(_stage_run(context, run_args, config, status))
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
    with _BatchStatus(runs, args) as status:
        for run in runs:
            try:
                _exec_and_finalize(run, run_args, status)
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
