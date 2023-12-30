# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

from .run_util import run_status
from .run_util import run_summary
from .run_util import run_user_attrs

__all__ = ["string_match_filter"]


def string_match_filter(s: str) -> RunFilter:
    if not s:
        raise ValueError("filter string cannot be empty")

    def f(run: Run):
        if not s:
            return False
        return (
            _match_op_name(run, s)
            or _match_run_status(run, s)
            or _match_user_attrs(run, s)
            or _match_summary_attrs(run, s)
        )

    return f


def _match_op_name(run: Run, s: str):
    return run.opref.op_name == s


def _match_run_status(run: Run, s: str):
    return run_status(run) == s


def _match_user_attrs(run: Run, s: str):
    attrs = run_user_attrs(run)
    return _match_label(attrs, s) or _match_tags(attrs, s)


def _match_label(attrs: dict[str, Any], s: str):
    return s in attrs.get("label", "")


def _match_tags(attrs: dict[str, Any], s: str):
    # TODO - implement when tags are supported
    return False


def _match_summary_attrs(run: Run, s: str):
    attrs = run_summary(run).get_run_attrs()
    return s in attrs.get("label", "")
