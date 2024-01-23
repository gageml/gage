# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import datetime
import logging
import operator

from functools import cmp_to_key

from .project_util import load_project_data
from .run_util import *

from .util import kebab_to_camel

log = logging.getLogger(__name__)

__ALL__ = [
    "BoardConfigError",
    "MissingGroupBy",
    "MissingGroupSelector",
    "MissingGroupSelectorField",
    "board_data",
    "filter_board_runs",
    "load_board",
]

_ColDef = dict[str, Any]
_ColDefs = list[_ColDef]
_ExtraColAttrs = dict[str, Any]
_Row = dict[str, Any]
_RowData = list[dict[str, Any]]


class BoardConfigError(Exception):
    pass


class MissingGroupBy(BoardConfigError):
    pass


class MissingGroupSelector(BoardConfigError):
    pass


class MissingGroupSelectorField(BoardConfigError):
    pass


def load_board_def(config_filename: str):
    data = load_project_data(config_filename)
    if not isinstance(data, dict):
        raise ValueError("expected a map")
    return BoardDef(data)


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
    status: str | None, filters: list[Callable[[Run], bool]]
):
    if status:
        filters.append(lambda run: run_status(run) == status)


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
        "status": run_status(run),
        "started": _run_datetime(run, "started"),
        "label": run_label(run),
    }
    return _gen_fields(fields, "run", field_cols)


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
    keys = {
        "id": 0,
        "name": 1,
        "operation": 2,
        "started": 3,
        "status": 4,
        "label": 5,
    }
    return keys.get(name, 99)


def _board_col_defs(board: BoardDef, inferred_col_defs: _ColDefs):
    board_cols = board.get_columns()
    if not board_cols:
        return inferred_col_defs
    return [
        _apply_col_def_key_case(_merged_col_def(board_col, inferred_col_defs))
        for board_col in board_cols
    ]


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
    col_attrs = dict(config_col)  # copy
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
        raise MissingGroupBy()
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
    raise MissingGroupSelector()


def _one_row_select_f(
    field_spec: dict[str, Any],
    cmp: Callable[[Any, Any], bool],
    cmp_name: str,
) -> Callable[[_RowData], _Row]:
    field_val = _field_reader(field_spec)
    if not field_val:
        raise MissingGroupSelectorField()

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
    fields = set(col["field"] for col in col_defs)
    return [
        {name: val for name, val in row.items() if name in fields} for row in row_data
    ]


def _board_meta(board: BoardDef):
    meta: dict[str, Any] = {}
    _maybe_apply_board_meta(board.get_title(), "title", meta)
    _maybe_apply_board_meta(board.get_description(), "description", meta)
    return meta


def _maybe_apply_board_meta(val: Any, attr: str, meta: dict[str, Any]):
    if val is not None:
        meta[attr] = val
