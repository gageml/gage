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

from ..gagefile import gagefile_for_dir

from ..archive_util import find_archive_dir

from ..run_util import meta_config
from ..run_util import run_label
from ..run_util import run_project_dir
from ..run_util import run_status
from ..run_util import run_summary
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


def selected_runs(
    args: SelectRunsSupport,
    deleted: bool = False,
    archive_dir: str | None = None,
):
    runs = (
        _archive_runs(archive_dir, args) if archive_dir else _local_runs(args, deleted)
    )
    return _select_runs(runs, args), len(runs)


def _archive_runs(archive: str, args: SelectRunsSupport):
    return var.list_runs(
        archive,
        sort=["-timestamp"],
        filter=_runs_filter(args),
    )


def _local_runs(args: SelectRunsSupport, deleted: bool):
    return var.list_runs(
        sort=["-timestamp"],
        filter=_runs_filter(args),
        deleted=deleted,
    )


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
    project_namespace = project_util.project_namespace()
    table = cli.Table(
        *_table_cols(width, deleted, simplified),
        expand=True,
        caption_justify="left",
        **table_kw,
    )
    for index, run in runs:
        table.add_row(*_table_row(index, run, width, project_namespace, simplified))
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
            "description",
            {
                "ratio": 1,
                "no_wrap": True,
                "style": _col_style("description", deleted),
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
        case "description", False:
            return None
        case _, False:
            assert False, (name, deleted)
        case _, True:
            return "dim"


def _table_row(
    index: int,
    run: Run,
    width: int,
    project_namespace: str | None,
    simplified: bool,
) -> list[str]:
    index_str = str(index)
    run_name = run.name[:5]
    operation = _run_operation(run, project_namespace)
    started = run_timestamp(run, "started")
    started_str = human_readable.date_time(started) if started else ""
    status = _run_status(run)
    description = _run_description(run, width)
    row = [
        index_str,
        run_name,
        operation,
        started_str,
        status,
        description,
    ]

    if simplified:
        return _simplify(row)
    return _fit(row, width)


def _run_operation(run: Run, project_namespace: str | None):
    opref = run.opref
    op_name = (
        opref.get_full_name() if opref.op_ns != project_namespace else opref.op_name
    )
    op_version = (
        f" [{cli.STYLE_SECOND_LABEL}]v{opref.op_version}[/]" if opref.op_version else ""
    )
    return f"{op_name}{op_version}"


def _run_status(run: Run):
    status = run_status(run)
    return cli.text(status, style=cli.run_status_style(status))


def _run_description(run: Run, width: int):
    summary = run_summary(run)
    config = meta_config(run)
    label = run_label(run)
    fields = {
        **summary.get_metrics(),
        **summary.get_attributes(),
        **config,
    }
    assigns = [
        (name, format_summary_value(fields.get(name)))
        for name in _run_desc_field_names(run, fields)
    ]
    if label:
        label_width = min(len(label), int(0.25 * width))
        rest_width = width - label_width
        formatted = cli.format_assigns(
            assigns,
            rest_width,
            ("cyan1", "bright_black", "dim"),
        )
        return f"[dim]{label}[/dim] {formatted}"
    else:
        return cli.format_assigns(
            assigns,
            width,
            ("cyan1", "bright_black", "dim"),
        )


__run_desc_field_name_cache: dict[str, list[str]] = {}


def _run_desc_field_names(run: Run, fields: dict[str, Any]):
    try:
        names = __run_desc_field_name_cache[run.id]
    except KeyError:
        __run_desc_field_name_cache[run.id] = names = (
            _try_run_desc_field_names(run) or []
        )
    return names or sorted(fields)


def _try_run_desc_field_names(run: Run):
    project_dir = run_project_dir(run)
    if not project_dir:
        return None
    try:
        gf = gagefile_for_dir(project_dir)
    except FileNotFoundError:
        return None
    opdef = gf.as_json().get(run.opref.op_name)
    if not opdef:
        return None
    fields = opdef.get("listing", {}).get("description")
    if not fields and not isinstance(fields, list):
        return None
    return cast(list[str], fields)


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


# =================================================================
# Archive support
# =================================================================


def archive_dir(archive_name: str):
    dirname = find_archive_dir(archive_name)
    if not dirname:
        cli.exit_with_error(
            f"archive '{archive_name}' does not exist\n\n"  # \
            "Use '[cmd]gage archive --list[/]' to show available archives."
        )
    return dirname
