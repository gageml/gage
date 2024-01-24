# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

from .board_impl import Args as BoardArgs
from .board_impl import _init_board
from .board_impl import _board_runs
from .board_impl import _board_data


class Args(NamedTuple):
    runs: list[str]
    where: str
    config: str


def publish(args: Args):
    board_args = BoardArgs(args.runs, args.where, args.config, True, False)
    board = _init_board(board_args)
    runs = _board_runs(board, board_args)
    data = _board_data(board, runs)
    print(len(data))
