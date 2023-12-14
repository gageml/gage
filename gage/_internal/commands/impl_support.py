# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import human_readable

from .. import cli
from .. import var
from .. import run_select

from ..run_util import meta_opref
from ..run_util import run_status
from ..run_util import run_timestamp
from ..run_util import run_user_attrs


__all__ = [
    "runs_table",
    "selected_runs",
    "one_run",
]

# =================================================================
# One run
# =================================================================


class OneRunSupport(Protocol):
    @property
    def run(self) -> str:
        ...


def one_run(args: OneRunSupport | str):
    run = args if isinstance(args, str) else args.run
    sorted = var.list_runs(sort=["-timestamp"])
    selected = run_select.select_runs(sorted, [run or "1"])
    if not selected:
        cli.err(
            f"No runs match {run!r}\n\n"  # \
            "Use '[cmd]gage list[/]' to show available runs."
        )
        raise SystemExit()
    if len(selected) > 1:
        assert False, "TODO: matches many runs, show list with error"

    return selected[0]


# =================================================================
# Selected runs
# =================================================================


class SelectRunsSupport(Protocol):
    @property
    def runs(self) -> list[str]:
        ...

    @property
    def where(self) -> str:
        ...


def selected_runs(args: SelectRunsSupport, deleted: bool = False):
    sorted = var.list_runs(sort=["-timestamp"], deleted=deleted)
    filtered = _filter_runs(sorted, args)
    selected = _select_runs(filtered, args)
    return selected, len(filtered)


def _filter_runs(runs: list[Run], args: SelectRunsSupport):
    # TODO apply where filter
    return runs


def _select_runs(runs: list[Run], args: SelectRunsSupport):
    if not args.runs:
        return [(i + 1, run) for i, run in enumerate(runs)]
    index_lookup = {run: i + 1 for i, run in enumerate(runs)}
    selected = run_select.select_runs(runs, args.runs)
    return [(index_lookup[run], run) for run in selected]


# =================================================================
# Runs table
# =================================================================


def runs_table(
    runs: list[tuple[int, Run]],
    deleted: bool = False,
    simplified: bool = False,
    **table_kw: Any,
):
    width = cli.console_width()
    table = cli.Table(
        *_table_cols(width, deleted, simplified),
        expand=True,
        caption_justify="left",
        **table_kw,
    )
    for index, run in runs:
        table.add_row(*_table_row(index, run, width, simplified))
    return table


_TRUNC_POINTS = [
    (28, (2, 3, 4, 5)),
    (45, (3, 4, 5)),
    (60, (5,)),
]


def _table_cols(width: int, deleted: bool, simplified: bool) -> list[cli.ColSpec]:
    headers = [
        (
            "#",
            {
                "ratio": None,
                "no_wrap": True,
                "style": _col_style("#", deleted),
            },
        ),
        (
            "name",
            {
                "ratio": None,
                "no_wrap": True,
                "style": _col_style("name", deleted),
            },
        ),
        (
            "operation",
            {
                "ratio": None,
                "no_wrap": True,
                "style": _col_style("operation", deleted),
            },
        ),
        (
            "started",
            {
                "ratio": None,
                "no_wrap": True,
                "style": _col_style("started", deleted),
            },
        ),
        (
            "status",
            {
                "ratio": None,
                "no_wrap": True,
                "style": _col_style("status", deleted),
            },
        ),
        (
            "label",
            {
                "ratio": 1,
                "no_wrap": True,
                "style": _col_style("label", deleted),
            },
        ),
    ]
    if simplified:
        return _simplify(headers)
    return _fit(headers, width)


_SIMPLE = [0, 2, 4, 5]


def _simplify(items: list[Any]):
    return [x for i, x in enumerate(items) if i in _SIMPLE]


def _col_style(name: str, deleted: bool):
    match name, deleted:
        case "#", False:
            return cli.STYLE_TABLE_HEADER
        case "#", True:
            return f"strike dim {cli.STYLE_TABLE_HEADER}"
        case "name", False:
            return "dim"
        case "operation", False:
            return cli.STYLE_LABEL
        case "started", False:
            return "dim"
        case "status", False:
            return ""
        case "label", False:
            return cli.STYLE_SECOND_LABEL
        case _, False:
            assert False, (name, deleted)
        case _, True:
            return "dim"


def _table_row(index: int, run: Run, width: int, simplified: bool) -> list[str]:
    opref = meta_opref(run)
    index_str = str(index)
    run_name = run.name[:5]
    op_name = opref.get_full_name()
    started = run_timestamp(run, "started")
    started_str = human_readable.date_time(started) if started else ""
    status = run_status(run)
    user_attrs = run_user_attrs(run)
    label = user_attrs.get("label") or ""

    row = [
        index_str,
        run_name,
        op_name,
        started_str,
        cli.text(status, style=cli.run_status_style(status)),
        label,
    ]

    if simplified:
        return _simplify(row)
    return _fit(row, width)


def _fit(l: list[Any], width: int):
    for limit, drop in _TRUNC_POINTS:
        if width <= limit:
            return [x for i, x in enumerate(l) if i not in drop]
    return l
