# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import re

from .util import find_apply

__all__ = [
    "select_runs",
]


def select_runs(runs: list[Run], select_specs: list[str]):
    selected = set()
    for spec in select_specs:
        selected.update(_select_runs(runs, spec))
    return [run for run in runs if run in selected]


def _select_runs(runs: list[Run], spec: str) -> list[Run]:
    return (
        find_apply(
            [
                _select_index,
                _select_slice,
                _select_id_or_name,
            ],
            runs,
            spec,
        )
        or []
    )


def _select_index(runs: list[Run], spec: str):
    try:
        index = int(spec)
    except ValueError:
        return None
    else:
        return [runs[index - 1]] if index >= 1 and index <= len(runs) else None


def _select_slice(runs: list[Run], spec: str):
    try:
        slice_start, slice_end = _parse_slice(spec)
    except ValueError:
        return None
    else:
        return runs[slice_start:slice_end]


def _parse_slice(spec: str):
    m = re.match("(-?\\d+)?:(-?\\d+)?", spec)
    if m:
        start, end = m.groups()
        return _slice_start(start), _slice_end(end)
    raise ValueError(spec) from None


def _slice_start(s: str | None):
    if not s:
        return None
    i = int(s)
    return i - 1 if i > 0 else i


def _slice_end(s: str | None):
    if not s:
        return None
    return int(s)


def _select_id_or_name(runs: list[Run], spec: str):
    return [run for run in runs if run.id.startswith(spec) or run.name.startswith(spec)]
