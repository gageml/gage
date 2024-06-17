# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

from .run_output import Progress, ProgressParser

import re

__all__ = ["progress_parser"]


def progress_parser(name: str) -> ProgressParser:
    match name:
        case "tqdm":
            return _parse_tqdm
        case _:
            raise ValueError(name)


_TQDM_EMPTY = re.compile(rb"^\s*$")
_TQDM_PROGRESS = re.compile(rb"\s*(\d+)%\|")


def _parse_tqdm(output: bytes) -> tuple[bytes, Progress | None]:
    """Parses output for tqdm progress.

    Returns tuple of progress-stripped output and Progress, if progress
    can be determined for output.
    """
    progress = None
    stripped_output_parts = []
    for part in output.split(b"\r"):
        if not part or _TQDM_EMPTY.match(part):
            continue
        progress_m = _TQDM_PROGRESS.match(part)
        if progress_m:
            progress = int(progress_m.group(1))
        else:
            stripped_output_parts.append(part)
    stripped_output = b"".join(stripped_output_parts)
    if progress is not None:
        return stripped_output, Progress(progress)
    return stripped_output, None
