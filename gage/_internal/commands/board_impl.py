# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import datetime
import json
import logging

from .. import cli

from ..run_util import *

from .impl_support import selected_runs

log = logging.getLogger(__name__)


class Args(NamedTuple):
    runs: list[str]
    where: str
    json: bool


def show_board(args: Args):
    if not args.json:
        cli.err(
            "You must specify --json for this command. Graphical "
            "boards are not yet supported."
        )
        raise SystemExit()
    runs, total_runs = selected_runs(args)
    attribute_defs = {}
    metric_defs = {}
    row_data = [
        {
            "__id__": run.id,
            "run:id": run.id,
            "run:name": run.name,
            "run:operation": run.opref.op_name,
            "run:status": run_status(run),
            "run:started": _run_datetime(run, "started"),
            "run:stopped": _run_datetime(run, "stopped"),
            **_summary_fields(
                summary.get_attributes(), "attribute", run, attribute_defs
            ),
            **_summary_fields(summary.get_metrics(), "metric", run, metric_defs),
        }
        for run, summary in [(run, run_summary(run)) for index, run in runs]
    ]
    column_defs = [
        {"field": "run:id"},
        {"field": "run:name"},
        {"field": "run:operation"},
        {"field": "run:status"},
        {"field": "run:started"},
        {"field": "run:stopped"},
        *[col_def for key, col_def in sorted(attribute_defs.items())],
        *[col_def for key, col_def in sorted(metric_defs.items())],
    ]
    data = {"columnDefs": column_defs, "rowData": row_data}
    cli.out(json.dumps(data, indent=2, sort_keys=True))


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


def _summary_fields(
    data: dict[str, Any],
    summary_type: str,
    run: Run,
    col_defs: dict[str, Any],
):
    fields: dict[str, JSONCompatible] = {}
    for key, val in data.items():
        field_name = f"{summary_type}:{key}"
        if isinstance(val, dict):
            val = val.get("value", None)
        if not _is_json_serializable(val):
            log.warning(
                "value for %s %s in run %s is not serializable, ignoring",
                key,
                summary_type,
                run.id,
            )
            continue
        col_defs[field_name] = {"field": field_name}
        fields[field_name] = val
    return fields


def _is_json_serializable(val: Any):
    # Pay the price of encoding to JSON otherwise risk failing for the
    # entire board.
    try:
        json.dumps(val)
    except TypeError:
        return False
    else:
        return True
