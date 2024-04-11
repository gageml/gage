# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

from .run_util import *

from . import var

__ALL__ = ["runs", "write_summary"]


def _run_attributes(run: Run) -> dict[str, Any]:
    return {
        name: _attr_val(value)
        for name, value in run_summary(run).get_attributes().items()
    }


def _attr_val(value: Any) -> Any:
    if isinstance(value, dict):
        return value.get("value")
    return value


def _run_metrics(run: Run) -> dict[str, Any]:
    return {
        name: _attr_val(value) for name, value in run_summary(run).get_metrics().items()
    }


_readers = {
    "attributes": _run_attributes,
    "config": meta_config,
    "label": run_label,
    "metrics": _run_metrics,
    "operation": lambda run: run.opref.op_name,
    "started": lambda run: run_attr(run, "started"),
    "status": run_status,
    "stopped": lambda run: run_attr(run, "stopped"),
}


class _Run:
    def __init__(self, run: Run):
        self._run = run
        self._cache = {}

    def __getitem__(self, name: str) -> Any:
        try:
            return self._cache[name]
        except KeyError:
            self._cache[name] = val = _readers[name](self._run)
            return val

    def get(self, name: str, default: Any = None):
        try:
            return self[name]
        except KeyError:
            return default


def runs(filter: Callable[[RunProxy], bool] | None = None) -> list[RunProxy]:
    return [run for run in _api_runs() if not filter or filter(run)]


def _api_runs():
    return [_Run(run) for run in var.list_runs(sort=["-timestamp"])]


def write_summary(
    metrics: dict[str, Any],
    attributes: dict[str, Any] | None = None,
    filename: str = "summary.json",
    echo: bool = True,
):
    import json

    if echo:
        _echo_summary(metrics, attributes)

    with open(filename, "w") as f:
        json.dump(
            {"metrics": metrics, "attributes": attributes},
            f,
            indent=True,
        )


def _echo_summary(metrics: dict[str, Any], attributes: dict[str, Any] | None = None):
    attributes = attributes or {}
    for summary in (metrics, attributes):
        for name, val in sorted(summary.items()):
            print(f"{name}: {val}")
