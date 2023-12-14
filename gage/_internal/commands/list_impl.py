# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

from .. import cli

from .impl_support import runs_table
from .impl_support import selected_runs


class Args(NamedTuple):
    runs: list[str]
    more: int
    limit: int
    all: bool
    where: str
    deleted: bool
    simplified: bool


def runs_list(args: Args):
    selected, from_count = selected_runs(args, args.deleted)
    limited = _limit_runs(selected, args)
    caption = _table_caption(len(limited), from_count, args)
    table = runs_table(
        limited,
        deleted=args.deleted,
        simplified=args.simplified,
        caption=caption,
    )
    cli.out(table)


def _limit_runs(runs: list[tuple[int, Run]], args: Args):
    if args.all:
        return runs
    limit = (args.more + 1) * args.limit
    return runs[:limit]


def _table_caption(shown_count: int, from_count: int, args: Args):
    if shown_count == from_count:
        return None
    assert shown_count < from_count, (shown_count, from_count)
    more_help = (
        f" [bright_black i](use -{'m' * (args.more + 1)} to show more)"
        if not args.runs
        else ""
    )
    return cli.pad(
        cli.markup(f"[dim i]Showing {shown_count} of {from_count} runs[/]{more_help}"),
        (0, 1),
    )
