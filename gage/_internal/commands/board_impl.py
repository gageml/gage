# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import datetime
import json
import logging
import operator

from .. import cli
from .. import util

from ..run_util import *
from ..project_util import load_data

from .impl_support import selected_runs

log = logging.getLogger(__name__)

_ColDef = dict[str, Any]
_ColDefs = list[_ColDef]
_RowData = list[dict[str, Any]]


class Args(NamedTuple):
    runs: list[str]
    where: str
    config: str
    json: bool


def show_board(args: Args):
    if not args.json:
        cli.exit_with_error(
            "You must specify --json for this command. Graphical "
            "boards are not yet supported."
        )
    board = _board_config(args.config)
    runs = _board_runs(board, args)
    _print_board_json_and_exit(board, runs)


def _board_config(config: str) -> BoardDef:
    if not config:
        return BoardDef({})
    try:
        data = load_data(config)
    except FileNotFoundError:
        cli.exit_with_error(f"Config file \"{config}\" does not exist")
    else:
        if not isinstance(data, dict):
            cli.exit_with_error(
                f"Unexpected board config in \"{config}\" - expected a map"
            )
        return BoardDef(data)


def _board_runs(board: BoardDef, args: Args):
    runs, total = selected_runs(args)
    run_filter = _board_run_filter(board)
    group_filter = _board_group_filter(board)
    return group_filter([run for index, run in runs if run_filter(run)])


def _board_run_filter(board: BoardDef) -> Callable[[Run], bool]:
    filters: list[Callable[[Run], bool]] = []
    run_select = board.get_run_select()
    _maybe_apply_op_filter(run_select.get_operation(), filters)
    _maybe_apply_status_filter(run_select.get_status(), filters)
    if not filters:
        return lambda run: True
    return lambda run: all(f(run) for f in filters)


def _maybe_apply_op_filter(op: str | None, filters: list[Callable[[Run], bool]]):
    if op:
        filters.append(lambda run: run.opref.op_name == op)


def _maybe_apply_status_filter(
    status: str | None, filters: list[Callable[[Run], bool]]
):
    if status:
        filters.append(lambda run: run_status(run) == status)


def _board_group_filter(board: BoardDef) -> Callable[[list[Run]], list[Run]]:
    group = board.get_run_select().get_group()
    if not group:
        return lambda runs: runs
    return lambda runs: _filter_grouped_runs(runs, group)


def _filter_grouped_runs(runs: list[Run], group_select: dict[str, Any]):
    groups = _group_runs(runs, group_select)
    return _select_from_groups(groups, group_select)


_RunGroupFunc = Callable[[Run], Any]


def _group_runs(runs: list[Run], group_select: dict[str, Any]):
    groups: dict[Any, list[Run]] = {}
    run_group = _run_group_f(group_select)
    for run in runs:
        groups.setdefault(run_group(run), []).append(run)
    return [runs for key, runs in sorted(groups.items())]


def _run_group_f(group_select: dict[str, Any]):
    f = util.find_apply([_run_attribute_group_f], group_select)
    if not f:
        cli.exit_with_error(
            "run-select group for board does not specify a field attribute: "
            "expected 'attribute', 'metric', 'run-attr', or 'config'"
        )
    return f


def _run_attribute_group_f(group_select: dict[str, Any]) -> _RunGroupFunc | None:
    name = group_select.get("attribute")
    if name:
        return lambda run: _field_val(run_summary(run).get_attributes().get(name))


def _field_val(val: Any) -> Any:
    return val.get("value") if isinstance(val, dict) else val


def _select_from_groups(groups: list[list[Run]], group_select: dict[str, Any]):
    # Only supporting select of latest runs - generalized facility to
    # come later
    started_func = group_select.get("started")
    if started_func not in ["last", "first"]:
        cli.exit_with_error(
            "run-select group must specify 'last' or 'first' for the 'started' "
            "attribute - this is a temporary limitation"
        )
    return [_cmp_started(group, cast(str, started_func)) for group in groups]


def _cmp_started(runs: list[Run], started_func: str):
    cmp = operator.ge if started_func == "last" else operator.lt
    latest: tuple[Run, datetime.datetime] | None = None
    for run in runs:
        started = run_attr(run, "started")
        if latest is None or cmp(started, latest[1]):
            latest = run, started
    assert latest
    return latest[0]


def _print_board_json_and_exit(board: BoardDef, runs: list[Run]):
    raw_col_defs, row_data = _board_raw_data(runs)
    col_defs = _board_col_defs(board, raw_col_defs)
    data = {
        **_board_meta(board),
        "colDefs": col_defs,
        "rowData": row_data,
    }
    cli.out(json.dumps(data, indent=2, sort_keys=True))
    raise SystemExit(0)


def _board_meta(board: BoardDef):
    meta: dict[str, Any] = {}
    _maybe_apply_board_meta(board.get_title(), "title", meta)
    _maybe_apply_board_meta(board.get_description(), "description", meta)
    return meta


def _maybe_apply_board_meta(val: Any, attr: str, meta: dict[str, Any]):
    if val is not None:
        meta[attr] = val


def _board_raw_data(runs: list[Run]):
    field_cols = {}
    row_data: _RowData = [
        {
            "__run__": {
                "id": run.id,
                "name": run.name,
                "operation": run.opref.op_name,
                "status": run_status(run),
                "started": _run_datetime(run, "started"),
                "stopped": _run_datetime(run, "stopped"),
            },
            **_config_fields(run, field_cols),
            **_summary_fields(run, field_cols),
        }
        for run, summary in [(run, run_summary(run)) for run in runs]
    ]
    col_defs: _ColDefs = [
        *_run_attr_cols(),
        *_sorted_field_cols(field_cols),
    ]
    return col_defs, row_data


def _run_datetime(run: Run, attr_name: str):
    val = run_attr(run, attr_name, None)
    if val is None:
        return None
    if not isinstance(val, datetime.datetime):
        log.warning(
            "unexpected datetime value for %s in run %s: %r", attr_name, run.id, val
        )
        return None
    return val.isoformat()


def _run_attr_cols():
    return [
        {"field": "run:status"},
        {"field": "run:id"},
        {"field": "run:name"},
        {"field": "run:operation"},
        {"field": "run:started"},
        {"field": "run:stopped"},
    ]


def _sorted_field_cols(field_cols: dict[str, Any]) -> _ColDefs:
    return [col for key, col in sorted(field_cols.items(), key=_field_col_sort_key)]


def _field_col_sort_key(kv: tuple[str, Any]):
    parts = kv[0].split(":", 1)
    match parts[0]:
        case "attribute":
            return (1, parts)
        case "metric":
            return (2, parts)
        case "config":
            return (0, parts)
        case _:
            assert False, kv


def _config_fields(run: Run, field_cols: dict[str, Any]):
    return _gen_fields(meta_config(run), "config", run, field_cols)


def _summary_fields(run: Run, field_cols: dict[str, Any]):
    summary = run_summary(run)
    return {
        **_gen_fields(summary.get_attributes(), "attribute", run, field_cols),
        **_gen_fields(summary.get_metrics(), "metric", run, field_cols),
    }


def _gen_fields(
    data: dict[str, Any],
    summary_type: str,
    run: Run,
    field_cols: dict[str, Any],
):
    fields: dict[str, JSONCompatible] = {}
    for key, val in data.items():
        field_name = f"{summary_type}:{key}"
        if isinstance(val, dict):
            attrs = val
            val = val.get("value", None)
        else:
            attrs = {}
        if _is_nan(val):
            val = None
        if not _is_json_serializable(val):
            log.warning(
                "value for %s %s in run %s is not serializable, ignoring",
                key,
                summary_type,
                run.id,
            )
            continue
        _apply_field_col(field_name, attrs, field_cols)
        fields[field_name] = val
    return fields


def _is_nan(val: Any):
    return isinstance(val, float) and val != val


def _apply_field_col(field_name: str, attrs: dict[str, Any], col_defs: dict[str, Any]):
    col_def = col_defs.setdefault(field_name, {})
    col_def["field"] = field_name
    try:
        col_def["headerName"] = attrs["label"]
    except KeyError:
        pass


def _is_json_serializable(val: Any):
    # Pay the price of encoding to JSON otherwise risk failing for the
    # entire board.
    try:
        json.dumps(val)
    except TypeError:
        return False
    else:
        return True


def _board_col_defs(board: BoardDef, col_defs: _ColDefs):
    config_cols: list[BoardDefColumn] | None = board.get_columns()
    if not config_cols:
        return col_defs
    return [_board_col_def(config_col, col_defs) for config_col in config_cols]


def _board_col_def(config_col: BoardDefColumn, col_defs: _ColDefs) -> dict[str, Any]:
    col_def, config_attrs = _find_col_def(config_col, col_defs)
    return {**col_def, **config_attrs} if col_def else config_attrs


def _find_col_def(config_col: BoardDefColumn, col_defs: _ColDefs):
    field_target, config_attrs = _field_target(config_col)
    if not field_target:
        return None, config_attrs
    for col_def in col_defs:
        if col_def["field"] == field_target:
            return col_def, config_attrs
    return {"field": field_target}, config_attrs


_ExtraColAttrs = dict[str, Any]


def _field_target(config_col: BoardDefColumn) -> tuple[str | None, _ExtraColAttrs]:
    col_attrs = dict(config_col)
    attribute = col_attrs.pop("attribute", None)
    if attribute:
        return f"attribute:{attribute}", col_attrs
    metric = col_attrs.pop("metric", None)
    if metric:
        return f"metric:{metric}", col_attrs
    config = col_attrs.pop("config", None)
    if config:
        return f"config:{config}", col_attrs
    run_attr = col_attrs.pop("run-attr", None)
    if run_attr:
        return f"run:{run_attr}", col_attrs
    return None, col_attrs
