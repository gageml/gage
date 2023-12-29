# SPDX-License-Identifier: Apache-2.0

from typing import *


__all__ = [
    "format_summary_value",
]


def format_summary_value(value: Any):
    if isinstance(value, dict):
        value = value.get("value", "")
    # TODO - smarter formatting
    return str(value)
