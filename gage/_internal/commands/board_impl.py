# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import csv
import io
import json
import logging
import sys

from .. import cli

from ..board import load_board_def
from ..board import default_board_config_path_for_dir

from ..board_util import *
from ..run_util import *

from .impl_support import selected_runs

log = logging.getLogger(__name__)


class Args(NamedTuple):
    runs: list[str]
    where: str
    config: str
    no_config: bool
    csv: bool
    json: bool


def show_board(args: Args):
    _check_args(args)
    board = _load_board_def(args)
    runs = _board_runs(board, args)
    data = _board_data(board, runs)
    if args.json:
        _print_json(data)
    elif args.csv:
        _print_csv(data)
    else:
        _print_table(data)


def _check_args(args: Args):
    if args.json and args.csv:
        cli.exit_with_error("You can't use both --json and --csv options.")
    if args.no_config and args.config:
        cli.exit_with_error("You can't use both --config and --no-config options.")


def _load_board_def(args: Args) -> BoardDef:
    filename = _config_filename(args)
    if not filename:
        log.info("Using default board config")
        return BoardDef("<default>", {})
    try:
        board_def = load_board_def(filename)
    except FileNotFoundError:
        cli.exit_with_error(f"Config file \"{filename}\" does not exist")
    except ValueError as e:
        cli.exit_with_error(f"Unexpected board config in \"{filename}\" - {e.args[0]}")
    else:
        log.info("Using config from %s", board_def.filename)
        return board_def


def _config_filename(args: Args):
    if args.config:
        return args.config
    elif args.no_config:
        return None
    else:
        try:
            return default_board_config_path_for_dir(".")
        except FileNotFoundError:
            return None


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
    except BoardConfigError as e:
        cli.exit_with_error(f"invalid board config: {e}")


def _print_json(data: dict[str, Any]):
    sys.stdout.write(json.dumps(data, indent=2, sort_keys=True))


def _print_csv(data: dict[str, Any]):
    col_defs = data["colDefs"]
    fields: list[str] = [col["field"] for col in col_defs]
    headers = {field: col.get("label") or field for field, col in zip(fields, col_defs)}
    buf = io.StringIO()
    writer = csv.DictWriter(buf, fields)
    writer.writerow(headers)
    for row in cast(list[dict[str, Any]], data["rowData"]):
        writer.writerow(_csv_row_values(row, fields))
    sys.stdout.write(buf.getvalue())


def _csv_row_values(row: dict[str, Any], fields: list[str]):
    return {key: _csv_cell_value(val) for key, val in row.items() if key in fields}


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
    cols = data["colDefs"]
    for col in cols:
        table.add_column(column_label(col))
    for row in data["rowData"]:
        table.add_row(*[formatted_cell_value(col, row) for col in cols])
    cli.out(table)
