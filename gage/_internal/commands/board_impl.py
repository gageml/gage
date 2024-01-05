# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import datetime
import json
import logging

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
        cli.err(
            "You must specify --json for this command. Graphical "
            "boards are not yet supported."
        )
        raise SystemExit()
    board = _board_config(args.config)
    runs = _board_runs(args)
    _print_board_json_and_exit(board, runs)


def _board_config(config: str) -> BoardDef:
    if not config:
        return BoardDef({})
    try:
        data = load_data(config)
    except FileNotFoundError:
        cli.err(f"Config file \"{config}\" does not exist")
        raise SystemExit()
    else:
        if not isinstance(data, dict):
            cli.err(f"Unexpected board config in \"{config}\" - expected a map")
            raise SystemExit()
        return BoardDef(data)


def _board_runs(args: Args):
    runs, total = selected_runs(args)
    return [run for index, run in runs]


def _print_board_json_and_exit(board: BoardDef, runs: list[Run]):
    raw_col_defs, row_data = _board_raw_data(runs)
    col_defs = _board_col_defs(board, raw_col_defs)
    data = {
        "title": board.get_title(),
        "description": board.get_description(),
        "colDefs": col_defs,
        "rowData": row_data,
    }
    cli.out(json.dumps(data, indent=2, sort_keys=True))
    raise SystemExit(0)


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
    field_m, config_attrs = _field_matcher(config_col)
    if not field_m:
        return None, config_attrs
    for col_def in col_defs:
        if field_m(col_def["field"]):
            return col_def, config_attrs
    return None, config_attrs


_FieldMatcher = Callable[[str], bool]
_ExtraColAttrs = dict[str, Any]


def _field_matcher(
    config_col: BoardDefColumn,
) -> tuple[_FieldMatcher | None, _ExtraColAttrs]:
    if isinstance(config_col, str):
        return _string_col_matcher(config_col)
    col_attrs = dict(config_col)
    attribute = col_attrs.pop("attribute", None)
    if attribute:
        return _attribute_col_matcher(attribute), col_attrs
    metric = col_attrs.pop("metric", None)
    if metric:
        return _metric_col_matcher(metric), col_attrs
    config = col_attrs.pop("config", None)
    if config:
        return _config_col_matcher(config), col_attrs
    run_attr = col_attrs.pop("run-attr", None)
    if run_attr:
        return _run_attr_col_matcher(run_attr), col_attrs
    return None, col_attrs


def _string_col_matcher(
    target_col: str,
) -> tuple[_FieldMatcher | None, _ExtraColAttrs]:
    if (
        target_col.startswith("attribute:")
        or target_col.startswith("metric:")
        or target_col.startswith("config:")
        or target_col.startswith("run:")
    ):
        return (lambda field_name: target_col == field_name), {}
    candidates = [
        "metric:" + target_col,
        "attribute:" + target_col,
        "config:" + target_col,
    ]
    return (lambda field_name: field_name in candidates), {}


def _attribute_col_matcher(attribute_name: str) -> _FieldMatcher:
    target = f"attribute:{attribute_name}"
    return lambda field_name: field_name == target


def _metric_col_matcher(metric_name: str) -> _FieldMatcher:
    target = f"metric:{metric_name}"
    return lambda field_name: field_name == target


def _config_col_matcher(key: str) -> _FieldMatcher:
    target = f"config:{key}"
    return lambda field_name: field_name == target


def _run_attr_col_matcher(attr_name: str) -> _FieldMatcher:
    target = f"run:{attr_name}"
    return lambda field_name: field_name == target
