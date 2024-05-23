# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

class Args(NamedTuple):
    runs: list[str]
    name: str
    delete: str
    edit: str
    list: bool
    where: str
    all: bool
    yes: bool


def archive(args: Args):
    print(f"TODO archive: {args}")
