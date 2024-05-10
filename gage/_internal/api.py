# SPDX-License-Identifier: Apache-2.0

from typing import *

import os

from .types import *

from .run_util import *
from .file_util import compare_paths

from . import var

__all__ = [
    "runs",
    "write_summary",
]


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


EchoFormat = Literal["flat", "json", "yaml"]


def write_summary(
    *,
    metrics: dict[str, Any],
    attributes: dict[str, Any] | None = None,
    filename: str = "summary.json",
    echo: bool = True,
    echo_format: EchoFormat = "flat",
    always_write: bool = False,
):

    summary = {
        **({"metrics": metrics} if metrics else {}),
        **({"attributes": attributes} if attributes else {}),
    }

    if echo:
        _echo_summary(summary, echo_format)

    if always_write or _is_cwd_run_dir():
        import json

        with open(filename, "w") as f:
            json.dump(summary, f, indent=2, sort_keys=True)


def _is_cwd_run_dir():
    run_dir = os.getenv("RUN_DIR")
    return run_dir and compare_paths(run_dir, os.getcwd())


def _echo_summary(summary: dict[str, Any], format: EchoFormat):
    if format == "flat":
        _echo_flat(summary)
    elif format == "json":
        _echo_json(summary)
    elif format == "yaml":
        _echo_yaml(summary)
    else:
        raise ValueError(format)


def _echo_flat(summary: dict[str, Any]):
    metrics = summary.get("metrics") or {}
    attributes = summary.get("attributes") or {}
    names = set(list(metrics) + list(attributes))
    missing = object()
    for name in sorted(names):
        metric = metrics.get(name, missing)
        attr = attributes.get(name, missing)
        if metric is not missing and attr is not missing:
            print(f"{name} (metric): {metric:g}")
            print(f"{name} (attribute): {attr}")
        elif metric is not missing:
            print(f"{name}: {metric:g}")
        elif attr is not missing:
            print(f"{name}: {attr}")
        else:
            assert False, (name, summary)


def _echo_json(summary: dict[str, Any]):
    import json

    print(json.dumps(summary, indent=2, sort_keys=True))


def _echo_yaml(summary: dict[str, Any]):
    import yaml

    print(yaml.dump(summary).rstrip())
