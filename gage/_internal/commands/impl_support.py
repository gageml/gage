# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import logging

import human_readable

from .. import cli
from .. import lang
from .. import project_util
from .. import run_select
from .. import var

from ..run_util import meta_opref
from ..run_util import run_label
from ..run_util import run_status
from ..run_util import run_timestamp

log = logging.getLogger(__name__)


__all__ = [
    "format_summary_value",
    "one_run",
    "one_run_for_spec",
    "runs_table",
    "selected_runs",
]


class OneRunSupport(Protocol):
    @property
    def run(self) -> str: ...

    @property
    def where(self) -> str: ...


class SelectRunsSupport(Protocol):
    @property
    def runs(self) -> list[str]: ...

    @property
    def where(self) -> str: ...


# =================================================================
# One run
# =================================================================


def one_run(args: OneRunSupport):
    sorted = var.list_runs(sort=["-timestamp"], filter=_runs_filter(args))
    selected = run_select.select_runs(sorted, [args.run or "1"])
    if not selected:
        cli.exit_with_error(
            f"No runs match {args.run!r}\n\n"  # \
            "Use '[cmd]gage list[/]' to show available runs."
        )
    if len(selected) > 1:
        assert False, "TODO: matches many runs, show list with error"

    return selected[0]


def one_run_for_spec(run: str):
    sorted = var.list_runs(sort=["-timestamp"])
    selected = run_select.select_runs(sorted, [run or "1"])
    if not selected:
        cli.exit_with_error(
            f"No runs match {run!r}\n\n"  # \
            "Use '[cmd]gage list[/]' to show available runs."
        )
    if len(selected) > 1:
        assert False, "TODO: matches many runs, show list with error"

    return selected[0]


def _runs_filter(args: OneRunSupport | SelectRunsSupport):
    if not args.where:
        return None
    try:
        log.debug("Filtering runs matching '%s'", args.where)
        return lang.parse_where_expr(args.where)
    except ValueError as e:
        cli.exit_with_error(f"Cannot use where expression {args.where!r}: {e}")


# =================================================================
# Selected runs
# =================================================================


def selected_runs(args: SelectRunsSupport, deleted: bool = False):
    runs = var.list_runs(
        sort=["-timestamp"],
        filter=_runs_filter(args),
        deleted=deleted,
    )
    selected = _select_runs(runs, args)
    return selected, len(runs)


def _select_runs(runs: list[Run], args: SelectRunsSupport) -> list[IndexedRun]:
    if not args.runs:
        return [(i + 1, run) for i, run in enumerate(runs)]
    index_lookup = {run.id: i + 1 for i, run in enumerate(runs)}
    selected = run_select.select_runs(runs, args.runs)
    return [(index_lookup[run.id], run) for run in selected]


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
    project_ns = project_util.project_ns()
    table = cli.Table(
        *_table_cols(width, deleted, simplified),
        expand=True,
        caption_justify="left",
        **table_kw,
    )
    for index, run in runs:
        table.add_row(*_table_row(index, run, width, project_ns, simplified))
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
            return "dim"
        case "name", False:
            return cli.STYLE_VALUE
        case "operation", False:
            return cli.STYLE_LABEL
        case "started", False:
            return cli.STYLE_VALUE
        case "status", False:
            return ""  # status style is value-dependent
        case "label", False:
            return cli.STYLE_SECOND_LABEL
        case _, False:
            assert False, (name, deleted)
        case _, True:
            return "dim"


def _table_row(
    index: int, run: Run, width: int, project_ns: str | None, simplified: bool
) -> list[str]:
    index_str = str(index)
    run_name = run.name[:5]
    op_name = _op_name(run, project_ns)
    started = run_timestamp(run, "started")
    started_str = human_readable.date_time(started) if started else ""
    status = run_status(run)
    label = run_label(run) or ""

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


def _op_name(run: Run, project_ns: str | None):
    opref = meta_opref(run)
    return opref.get_full_name() if opref.op_ns != project_ns else opref.op_name


def _fit(l: list[Any], width: int):
    for limit, drop in _TRUNC_POINTS:
        if width <= limit:
            return [x for i, x in enumerate(l) if i not in drop]
    return l


def format_summary_value(value: Any):
    if isinstance(value, dict):
        value = value.get("value", "")
    if isinstance(value, float):
        return f"{value:.4g}"
    if isinstance(value, str):
        return value
    if value is None:
        return ""
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, list):
        return ", ".join([format_summary_value(item) for item in value])
    return str(value)
