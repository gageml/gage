# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import datetime
import logging
import operator

from functools import cmp_to_key

from .run_util import *

from .summary_util import format_summary_value
from .util import kebab_to_camel

log = logging.getLogger(__name__)

__all__ = [
    "BoardConfigError",
    "board_data",
    "column_label",
    "filter_board_runs",
    "formatted_cell_value",
]

_ColDef = dict[str, Any]
_ColDefs = list[_ColDef]
_ExtraColAttrs = dict[str, Any]
_Row = dict[str, Any]
_RowData = list[dict[str, Any]]


def filter_board_runs(runs: list[Run], board: BoardDef):
    run_select = board.get_run_select()
    filter = _board_run_filter(run_select) if run_select else lambda run: True
    return [run for run in runs if filter(run)]


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
    status: str | list[str] | None,
    filters: list[Callable[[Run], bool]],
):
    if status:
        if isinstance(status, str):
            status = [status]
        filters.append(lambda run: run_status(run) in status)


def board_data(board: BoardDef, runs: list[Run]) -> dict[str, Any]:
    inferred_col_defs, raw_row_data = _board_raw_data(runs)
    col_defs = _board_col_defs(board, inferred_col_defs)
    row_data = _prune_row_data_fields(_filter_by_group(raw_row_data, board), col_defs)
    return {
        **_board_meta(board),
        "colDefs": col_defs,
        "rowData": row_data,
    }


def _board_raw_data(runs: list[Run]):
    field_cols = {}
    row_data: _RowData = [
        {
            **_run_attr_fields(run, field_cols),
            **_config_fields(run, field_cols),
            **_summary_fields(run, field_cols),
        }
        for run, summary in [(run, run_summary(run)) for run in runs]
    ]
    col_defs = _sorted_field_cols(field_cols)
    return col_defs, row_data


def _run_attr_fields(run: Run, field_cols: dict[str, Any]):
    fields = {
        "id": run.id,
        "name": run.name,
        "operation": run.opref.op_name,
        "op_ns": run.opref.op_ns,
        "op_version": run.opref.op_version,
        "status": run_status(run),
        "started": _run_datetime(run, "started"),
        "stopped": _run_datetime(run, "stopped"),
        "label": run_label(run),
    }
    return _gen_fields(fields, "run", field_cols)


_RUN_ATTR_ORDER = {
    key: i
    for i, key in enumerate(
        [
            "id",
            "name",
            "operation",
            "op_ns",
            "op_version",
            "started",
            "stopped",
            "status",
            "label",
        ]
    )
}

_RUN_ATTR_DEFAULT_COL_DEFS = {
    "name",
    "operation",
    "started",
    "status",
    "label",
}


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


def _gen_fields(
    data: dict[str, Any],
    summary_type: str,
    field_cols: dict[str, Any],
):
    fields: dict[str, JSONCompatible] = {}
    for key, val in data.items():
        field_name = f"{summary_type}:{key}"
        _apply_field_col(field_name, field_cols)
        fields[field_name] = _normalize_field_val(val)
    return fields


def _normalize_field_val(data: Any) -> Any:
    data = _safe_json_val(data)
    if isinstance(data, dict):
        try:
            val = data["value"]
        except KeyError:
            return data
        else:
            return val if len(data) == 1 else {**data, "value": val}
    return data


def _safe_json_val(data: Any) -> Any:
    if isinstance(data, dict):
        return {key: _safe_json_val(val) for key, val in data.items()}
    if isinstance(data, list):
        return [_safe_json_val(val) for val in data]
    return None if _is_nan(data) else data


def _is_nan(val: Any):
    return isinstance(val, float) and val != val


def _apply_field_col(field_name: str, col_defs: dict[str, Any]):
    col_def = col_defs.setdefault(field_name, {})
    col_def["field"] = field_name


def _config_fields(run: Run, field_cols: dict[str, Any]):
    return _gen_fields(meta_config(run), "config", field_cols)


def _summary_fields(run: Run, field_cols: dict[str, Any]):
    summary = run_summary(run)
    return {
        **_gen_fields(summary.get_attributes(), "attribute", field_cols),
        **_gen_fields(summary.get_metrics(), "metric", field_cols),
    }


def _sorted_field_cols(field_cols: dict[str, Any]) -> _ColDefs:
    return [col for key, col in sorted(field_cols.items(), key=_field_col_sort_key)]


def _field_col_sort_key(kv: tuple[str, Any]):
    parts = kv[0].split(":", 1)
    match parts[0]:
        case "run":
            return (0, _run_attr_sort_key(parts[1]), parts[0])
        case "config":
            return (1, parts[1])
        case "attribute":
            return (2, parts[1])
        case "metric":
            return (3, parts[1])
        case _:
            assert False, kv


def _run_attr_sort_key(name: str):
    return _RUN_ATTR_ORDER.get(name, 99)


def _board_col_defs(board: BoardDef, inferred_col_defs: _ColDefs):
    board_cols = board.get_columns()
    if not board_cols:
        return _filter_run_attr_cols_for_inferred(inferred_col_defs)
    return [
        _apply_col_def_key_case(_merged_col_def(board_col, inferred_col_defs))
        for board_col in board_cols
    ]


def _filter_run_attr_cols_for_inferred(col_defs: _ColDefs):
    return [col_def for col_def in col_defs if not _excluded_run_attr(col_def)]


def _excluded_run_attr(col_def: dict[str, Any]):
    field = col_def["field"]
    return field.startswith("run:") and field[4:] not in _RUN_ATTR_DEFAULT_COL_DEFS


def _apply_col_def_key_case(col_def: dict[str, Any]) -> dict[str, Any]:
    return {
        kebab_to_camel(key): (
            _apply_col_def_key_case(val) if isinstance(val, dict) else val
        )
        for key, val in col_def.items()
    }


def _merged_col_def(
    board_col: BoardDefColumn, inferred_col_defs: _ColDefs
) -> dict[str, Any]:
    col_def, inferred_attrs = _find_col_def(board_col, inferred_col_defs)
    return {**col_def, **inferred_attrs} if col_def else inferred_attrs


def _find_col_def(board_col: BoardDefColumn, inferred_col_defs: _ColDefs):
    field_target, rest_config = _field_target(board_col)
    if not field_target:
        return None, rest_config
    for inferred_col in inferred_col_defs:
        if inferred_col["field"] == field_target:
            return inferred_col, rest_config
    return {"field": field_target}, rest_config


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


def _filter_by_group(row_data: _RowData, board: BoardDef) -> _RowData:
    group_select = board.get_group_select()
    if not group_select:
        return row_data
    group_key = _group_key_f(group_select)
    select_from_group = _select_from_group_f(group_select)
    groups = _group_row_data(row_data, group_key)
    return [select_from_group(group) for group in groups]


def _group_key_f(group_select: BoardDefGroupSelect) -> Callable[[_Row], Any]:
    reader = _field_reader(group_select.get_group_by())
    if not reader:
        raise BoardConfigError(
            "group-select for board is missing group-by field: expected "
            "run-attr, attribute, metric, or config"
        )
    return reader


def _field_reader(field_spec: dict[str, Any]) -> Callable[[_Row], Any] | None:
    field_name = _field_name(field_spec)
    if not field_name:
        return None
    return lambda data: _field_value(field_name, data)


def _field_name(field_spec: dict[str, Any]):
    field_candidates: list[tuple[str, str]] = [
        ("attribute", "attribute:"),
        ("metric", "metric:"),
        ("run-attr", "run:"),
        ("config", "config:"),
    ]
    for type, prefix in field_candidates:
        try:
            name = field_spec[type]
        except KeyError:
            pass
        else:
            return prefix + name
    return None


def _field_value(field_name: str, data: Any) -> Any:
    val = data.get(field_name)
    return val.get("value") if isinstance(val, dict) else val


def _select_from_group_f(
    group_select: BoardDefGroupSelect,
) -> Callable[[_RowData], _Row]:
    min = group_select.get_min()
    if min:
        return _one_row_select_f(min, operator.lt, "min")
    max = group_select.get_max()
    if max:
        return _one_row_select_f(max, operator.gt, "max")
    raise BoardConfigError(
        "group-select for board must specify either min or max fields"
    )


def _one_row_select_f(
    field_spec: dict[str, Any],
    cmp: Callable[[Any, Any], bool],
    cmp_name: str,
) -> Callable[[_RowData], _Row]:
    field_val = _field_reader(field_spec)
    if not field_val:
        raise BoardConfigError(
            "group-select selector (min/max) for board is missing field: "
            "expected run-attr, attribute, metric, or config"
        )

    def f(row_data: _RowData) -> _Row:
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
    row_data: _RowData, group_key: Callable[[_Row], Any]
) -> list[_RowData]:
    groups: dict[Any, _RowData] = {}
    for data in row_data:
        groups.setdefault(group_key(data), []).append(data)
    return [
        group for key, group in sorted(groups.items(), key=cmp_to_key(_group_item_cmp))
    ]


def _group_item_cmp(lhs: tuple[Any, Any], rhs: tuple[Any, Any]):
    lhsKey = lhs[0]
    if lhsKey is None:
        return -1
    rhsKey = rhs[0]
    if rhsKey is None:
        return 1
    try:
        return -1 if lhsKey < rhsKey else 1 if lhsKey > rhsKey else 0
    except TypeError:
        return -1


def _prune_row_data_fields(row_data: _RowData, col_defs: _ColDefs):
    fields = set(col["field"] for col in col_defs if "field" in col)
    return [
        # Keep row data for all col def fields and for run ID (run ID
        # should always appear in row data)
        {name: val for name, val in row.items() if name in fields or name == "run:id"}
        for row in row_data
    ]


def _board_meta(board: BoardDef):
    meta: dict[str, Any] = {}
    _maybe_apply_meta(board.get_title(), "title", meta)
    _maybe_apply_meta(board.get_description(), "description", meta)
    group_col = board.get_group_column()
    if group_col:
        meta["groupColumn"] = _apply_col_def_key_case(group_col)
    return meta


def _maybe_apply_meta(val: Any, attr: str, meta: dict[str, Any]):
    if val is not None:
        meta[attr] = val


def column_label(column: BoardDefColumn):
    label = column.get("label")
    if label:
        return cast(str, label)
    try:
        field = column["field"]
    except KeyError:
        if "score" in column:
            return "score"
        else:
            return ""
    else:
        return _default_field_label(field)


def _default_field_label(field: str):
    if field.startswith("run:"):
        return field[4:]
    elif field.startswith("metric:"):
        return field[7:]
    elif field.startswith("attribute:"):
        return field[10:]
    elif field.startswith("config:"):
        return field[7:]
    else:
        return field


def formatted_cell_value(column: BoardDefColumn, row: dict[str, Any]):
    try:
        field = column["field"]
    except KeyError:
        try:
            score = column["score"]
        except KeyError:
            return ""
        else:
            return _table_row_score(column["score"], row)
    else:
        return _table_row_field_value(column["field"], row.get(field))


def _table_row_field_value(field: str, value: Any):
    if field.startswith("run:"):
        if field == "run:started" or field == "run:stopped":
            return _format_datetime(value)
        return format_summary_value(value)
    elif field.startswith("attribute:") and isinstance(value, str):
        return _try_format_datetime(value) or format_summary_value(value)
    else:
        return format_summary_value(value)


def _try_format_datetime(s: str):
    try:
        return _format_datetime(s)
    except ValueError:
        return None


def _format_datetime(s: str | None):
    if not s:
        return ""
    d = datetime.datetime.fromisoformat(s)
    return datetime.datetime.strftime(d, "%Y-%m-%d %H:%M")


def _table_row_score(score_config: Any, row: Any):
    try:
        average = score_config["average"]
    except KeyError:
        log.debug("Unsupported score config (expected 'average'): %s", score_config)
        return ""
    else:
        score = _average_score(average, row)
        return format_summary_value(score) if score is not None else ""


def _average_score(fields: list[str | tuple[str, float | int]], row: Any):
    vals: list[tuple[float, float]] = []
    for field in fields:
        try:
            vals.append(_num_field_val_and_weight(field, row))
        except (KeyError, TypeError, ValueError):
            pass
    if not vals:
        return None
    total_weight = sum(weight for val, weight in vals)
    return sum(val * weight / total_weight for val, weight in vals)


def _num_field_val_and_weight(
    field: str | tuple[str, float | int], row: dict[str, Any]
):
    if isinstance(field, str):
        field_name = field
        weight = 1
    else:
        field_name, weight = field
    val = row[field_name]
    if isinstance(val, dict):
        val = val["value"]
    return (float(val), float(weight))
