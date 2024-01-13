# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import datetime
import json
import logging
import operator

from .. import cli

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
    run_select = board.get_run_select()
    filter = _board_run_filter(run_select) if run_select else lambda run: True
    return [run for index, run in runs if filter(run)]


def _board_run_filter(run_select: BoardDefRunSelect) -> Callable[[Run], bool]:
    filters: list[Callable[[Run], bool]] = []
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


def _print_board_json_and_exit(board: BoardDef, runs: list[Run]):
    raw_col_defs, row_data = _board_raw_data(runs)
    col_defs = _board_col_defs(board, raw_col_defs)
    row_data = _filter_by_group(row_data, board)
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


def _filter_by_group(row_data: list[dict[str, Any]], board: BoardDef) -> _RowData:
    group_select = board.get_group_select()
    if not group_select:
        return row_data
    group_key = _group_key_f(group_select)
    select_from_group = _select_from_group_f(group_select)
    groups = _group_row_data(row_data, group_key)
    return [select_from_group(group) for group in groups]


def _group_key_f(group_select: BoardDefGroupSelect) -> Callable[[dict[str, Any]], Any]:
    reader = _field_reader(group_select.get_group_by())
    if not reader:
        cli.exit_with_error(
            "group-select for board is missing group-by field: expected "
            "run-attr, attribute, metric, or config"
        )
    return reader


def _field_reader(field_spec: dict[str, Any]) -> Callable[[dict[str, Any]], Any] | None:
    name = field_spec.get("run-attr")
    if name:
        return lambda data: data["__run__"].get(name)
    name = field_spec.get("attribute")
    if name:
        return lambda data: data.get(f"attribute:{name}")
    name = field_spec.get("metric")
    if name:
        return lambda data: data.get(f"metric:{name}")
    name = field_spec.get("config")
    if name:
        return lambda data: data.get(f"config:{name}")
    return None


def _select_from_group_f(
    group_select: BoardDefGroupSelect,
) -> Callable[[_RowData], dict[str, Any]]:
    min = group_select.get_min()
    if min:
        return _one_row_select_f(min, operator.lt, "min")
    max = group_select.get_max()
    if max:
        return _one_row_select_f(max, operator.gt, "max")
    cli.exit_with_error("group-select for board must specify either min or max fields")


def _one_row_select_f(
    field_spec: dict[str, Any],
    cmp: Callable[[Any, Any], bool],
    cmp_name: str,
) -> Callable[[_RowData], dict[str, Any]]:
    field_val = _field_reader(field_spec)
    if not field_val:
        cli.exit_with_error(
            f"group-select {cmp_name} for board is missing field: expected "
            "run-attr, attribute, metric, or config"
        )

    def f(row_data: _RowData) -> dict[str, Any]:
        assert row_data
        selected = None
        selected_val = None
        for cur in row_data:
            cur_val = field_val(cur)
            if selected is None or cmp(cur_val, selected_val):
                selected = cur
                selected_val = cur_val
        assert selected
        return selected

    return f


def _group_row_data(
    row_data: _RowData, group_key: Callable[[dict[str, Any]], Any]
) -> list[_RowData]:
    groups: dict[Any, _RowData] = {}
    for data in row_data:
        groups.setdefault(group_key(data), []).append(data)
    return [group for key, group in sorted(groups.items())]
