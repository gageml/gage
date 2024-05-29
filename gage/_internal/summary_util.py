# SPDX-License-Identifier: Apache-2.0

from typing import *

__all__ = ["format_summary_value"]


def format_summary_value(value: Any):
    if isinstance(value, dict):
        value = value.get("value", "")
    if isinstance(value, float):
        return f"{value:.4g}"
    if isinstance(value, str):
        return value
    if value is None:
        return ""
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, list):
        return ", ".join([format_summary_value(item) for item in value])
    return str(value)
