# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

from .run_output import Progress, ProgressParser

import re

__ALL__ = ["progress_parser"]


def progress_parser(name: str) -> ProgressParser:
    match name:
        case "tqdm":
            return _parse_tqdm
        case _:
            raise ValueError(name)


_TQDM_EMPTY = re.compile(rb" *\r$")
_TQDM_PROGRESS = re.compile(rb" *(\d+)%\|.+[\r\n]$")


def _parse_tqdm(output: bytes) -> tuple[bytes, Progress | None]:
    """Parses output for tqdm progress.

    Returns tuple of progress-stripped output and None.

    None part of tuple would otherwise be used to represent "progress"
    based on output, which could be presented to the user via the output
    callback interface. See `run_output` module for details.
    """
    if _TQDM_EMPTY.match(output):
        return b"", None
    m = _TQDM_PROGRESS.match(output)
    if m:
        return b"", Progress(int(m.group(1)))
    return output, None
