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


_TQDM_SPLIT_P = re.compile(rb"\r *\d+%|.+\r")
_TQDM_PART_P = re.compile(rb"\r *\d+%|.+]")


def _parse_tqdm(output: bytes) -> tuple[bytes, Progress | None]:
    """Parses output for tqdm progress.

    Returns tuple of progress-stripped output and None.

    None part of tuple would otherwise be used to represent "progress"
    based on output, which could be presented to the user via the output
    callback interface. See `run_output` module for details.
    """
    split = _TQDM_SPLIT_P.split(output)[-1]
    return (b"" if _TQDM_PART_P.match(split) else split), None
