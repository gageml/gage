# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import csv
import io
import json
import logging
import sys

from .. import cli
from ..board_util import *
from ..run_util import *

from .impl_support import format_summary_value, selected_runs

log = logging.getLogger(__name__)


class Args(NamedTuple):
    runs: list[str]
    where: str
    config: str
    csv: bool
    json: bool


def show_board(args: Args):
    if args.json and args.csv:
        cli.exit_with_error("You can't use both --json and --csv options.")
    board = _init_board(args)
    runs = _board_runs(board, args)
    data = _board_data(board, runs)
    if args.json:
        _print_json(data)
    elif args.csv:
        _print_csv(data)
    else:
        _print_table(data)


def _init_board(args: Args) -> BoardDef:
    if not args.config:
        return BoardDef({})
    try:
        log.info("Loading board from %s", args.config)
        return load_board_def(args.config)
    except FileNotFoundError:
        cli.exit_with_error(f"Config file \"{args.config}\" does not exist")
    except ValueError as e:
        cli.exit_with_error(
            f"Unexpected board config in \"{args.config}\" - {e.args[0]}"
        )


def _board_runs(board: BoardDef, args: Args):
    runs, total = selected_runs(args)
    filtered = filter_board_runs([run for index, run in runs], board)
    log.debug(
        "Selected %i run(s) for using command options; "
        "selected %i using board config criteria",
        len(runs),
        len(filtered),
    )
    return filtered


def _board_data(board: BoardDef, runs: list[Run]):
    log.info("Generating board data for %i run(s)", len(runs))
    try:
        return board_data(board, runs)
    except MissingGroupBy:
        cli.exit_with_error(
            "group-select for board is missing group-by field: expected "
            "run-attr, attribute, metric, or config"
        )
    except MissingGroupSelector:
        cli.exit_with_error(
            "group-select for board must specify either min or max fields"
        )
    except MissingGroupSelectorField:
        cli.exit_with_error(
            f"group-select selector (min/max) for board is missing field: "
            "expected run-attr, attribute, metric, or config"
        )


def _print_json(data: dict[str, Any]):
    sys.stdout.write(json.dumps(data, indent=2, sort_keys=True))


def _print_csv(data: dict[str, Any]):
    col_defs = data["colDefs"]
    fields: list[str] = [col["field"] for col in col_defs]
    headers = {
        field: col.get("label") or field
        for field, col in [(col["field"], col) for col in col_defs]
    }
    buf = io.StringIO()
    writer = csv.DictWriter(buf, fields)
    writer.writerow(headers)
    for row in cast(list[dict[str, Any]], data["rowData"]):
        writer.writerow(_csv_row_values(row))
    sys.stdout.write(buf.getvalue())


def _csv_row_values(row: dict[str, Any]):
    return {key: _csv_cell_value(val) for key, val in row.items()}


def _csv_cell_value(val: Any) -> Any:
    if isinstance(val, dict):
        return _csv_cell_value(val.get("value"))
    elif isinstance(val, list):
        return ",".join([_csv_cell_value(x) for x in val])
    elif val is True:
        return 1
    elif val is False:
        return 0
    else:
        return val


def _print_table(data: dict[str, Any]):
    table = cli.Table()
    fields = [(col["field"], col) for col in data["colDefs"]]
    for field, col in fields:
        table.add_column(col.get("label") or _table_default_field_label(field))
    for row in data["rowData"]:
        table.add_row(*[_format_summary_value(row[field]) for field, _ in fields])
    cli.out(table)


def _table_default_field_label(field: str):
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


def _format_summary_value(value: Any):
    if isinstance(value, str):
        try:
            d = datetime.datetime.fromisoformat(value)
        except ValueError:
            return value
        else:
            return datetime.datetime.strftime(d, "%Y-%m-%d %H:%M")
    return format_summary_value(value)
