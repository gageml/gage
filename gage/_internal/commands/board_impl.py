# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import csv
import io
import json
import logging

from .. import cli

from ..board_util import *
from ..run_util import *

from .impl_support import selected_runs

log = logging.getLogger(__name__)


class Args(NamedTuple):
    runs: list[str]
    where: str
    config: str
    csv: bool
    json: bool


def show_board(args: Args):
    if not args.json and not args.csv:
        cli.exit_with_error(
            "You must specify either --csv or --json for this command. "
            "Graphical boards are not yet supported."
        )
    if args.json and args.csv:
        cli.exit_with_error("You can't use both --json and --csv options.")
    board = _init_board(args)
    runs = _board_runs(board, args)
    data = _board_data(board, runs)
    if args.json:
        _print_json_and_exit(data)
    elif args.csv:
        _print_csv_and_exit(data)
    else:
        assert False


def _init_board(args: Args) -> BoardDef:
    if not args.config:
        return BoardDef({})
    try:
        return load_board_def(args.config)
    except FileNotFoundError:
        cli.exit_with_error(f"Config file \"{args.config}\" does not exist")
    except ValueError as e:
        cli.exit_with_error(
            f"Unexpected board config in \"{args.config}\" - {e.args[0]}"
        )


def _board_runs(board: BoardDef, args: Args):
    runs, total = selected_runs(args)
    return filter_board_runs([run for index, run in runs], board)


def _board_data(board: BoardDef, runs: list[Run]):
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


def _print_json_and_exit(data: dict[str, Any]):
    cli.out(json.dumps(data, indent=2, sort_keys=True))
    raise SystemExit(0)


def _print_csv_and_exit(data: dict[str, Any]):
    col_defs = data["colDefs"]
    fields: list[str] = [col["field"] for col in col_defs]
    headers = {
        field: col.get("label") or field
        for field, col in [(col["field"], col) for col in col_defs]
    }
    buf = io.StringIO()
    writer = csv.DictWriter(buf, fields)
    writer.writerow(headers)
    for row in data["rowData"]:
        writer.writerow(row)
    cli.out(buf.getvalue())
    raise SystemExit(0)
