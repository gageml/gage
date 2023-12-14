# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import datetime

from rich.console import Group
from rich.markdown import Markdown
from rich.padding import Padding
from rich.table import Table
from rich.table import Column

from .. import cli
from .. import util

from ..run_comment import *

from .impl_support import one_run
from .impl_support import selected_runs


class Args(NamedTuple):
    runs: list[str]
    msg: str
    delete: str
    edit: str
    list: bool
    where: str
    all: bool
    yes: bool


def comment(args: Args):
    if args.delete:
        _handle_delete(args)
    elif args.edit:
        _handle_edit(args)
    elif args.list:
        _handle_list(args)
    else:
        _handle_comment(args)


def _handle_delete(args: Args):
    if not args.runs and not args.all:
        args = Args(["1"], *args[1:])
    if len(args.runs) > 1 or args.all:
        cli.exit_with_error("Only one run can be specified with '-d / --delete'.\n\n")
    run = one_run(args.runs[0])
    comment = _run_comment(run, args.delete)
    _verify_delete(args, run, comment)
    delete_comment(run, args.delete)
    cli.err(f"Delete comment [arg]{comment.id}[/] from run [arg]{run.name}[/]")


def _run_comment(run: Run, comment_id: str):
    for comment in get_comments(run):
        if comment.id == comment_id:
            return comment
    cli.exit_with_error(
        f"Cannot find comment '{comment_id}' for run '{run.name}'.\n]n"
        f"Use '[cmd]gage comment --list {run.name}' to show run comments."
    )


def _verify_delete(args: Args, run: Run, comment: RunComment):
    if args.yes:
        return
    cli.err(CommentPanel(run, comment))
    cli.err(f"You are about to delete this comment.")
    cli.err()
    if not cli.confirm(f"Continue?", default=False):
        raise SystemExit(0)


def _handle_edit(args: Args):
    if not args.runs and not args.all:
        args = Args(["1"], *args[1:])
    if len(args.runs) > 1 or args.all:
        cli.exit_with_error("Only one run can be specified with '-e / --edit'.\n\n")
    run = one_run(args.runs[0])
    comment = _run_comment(run, args.edit)
    msg = _comment_message(args, comment.msg)
    if not msg:
        cli.exit_with_error("Comment was not modified.")
    set_comment(run, comment.id, msg)
    cli.err(f"Modified comment [arg]{comment.id}[/]")


def _handle_list(args: Args):
    if not args.runs and not args.all:
        args = Args(["1"], *args[1:])
    runs, from_count = selected_runs(args)
    for index, run in runs:
        for comment in get_comments(run):
            cli.out(CommentPanel(run, comment))


def CommentPanel(run: Run, comment: RunComment):
    attrs = Table.grid(
        Column(style=cli.STYLE_LABEL),
        Column(style=cli.STYLE_VALUE),
        padding=(0, 1),
        collapse_padding=False,
    )
    attrs.add_row("author", comment.author)
    attrs.add_row("date", _format_comment_timestamp(comment.timestamp))
    attrs.add_row("run", run.name)
    attrs.add_row("comment id", comment.id)
    return cli.Panel(
        Group(attrs, Padding(Markdown(comment.msg), (1, 0, 0, 0))),
        title=f"{comment.id} {run.name}",
    )


def _format_comment_timestamp(timestamp_ms: int):
    return datetime.datetime.fromtimestamp(timestamp_ms / 1000).strftime("%x %X")


def _handle_comment(args: Args):
    runs = _selected_runs_for_modify(args)
    msg = _comment_message(args)
    if not msg:
        cli.exit_with_error("Nothing added")
    for index, run in runs:
        add_comment(run, msg)
    added_to = (
        f"run [arg]{runs[0][1].name}[/]" if len(runs) == 1 else f"{len(runs)} runs"
    )
    cli.err(cli.markup(f"Added comment to {added_to}"))


def _selected_runs_for_modify(args: Args):
    if not args.runs and not args.all:
        cli.exit_with_error(
            "Specify a run to modify or use '--all'.\n\n"
            "Use '[cmd]gage list[/]' to show available runs.\n\n"
            "Try '[cmd]gage comment -h[/]' for additional help."
        )
    runs, from_count = selected_runs(args)
    if not runs:
        cli.exit_with_error("Nothing selected")
    return runs


def _comment_message(args: Args, default: str = ""):
    if args.msg:
        return args.msg
    msg = util.edit(default)
    return msg.strip() if msg else ""
