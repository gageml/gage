# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import datetime
import logging
import os

import human_readable

import rich.box

from rich.console import Group
from rich.padding import Padding
from rich.markdown import Markdown
from rich.text import Text
from rich.table import Table, Column

import human_readable

from natsort import natsorted

from .. import cli

from ..run_comment import get_comments
from ..run_output import RunOutputReader
from ..run_util import *

from ..util import format_user_dir

from .impl_support import format_summary_value
from .impl_support import one_run


log = logging.getLogger(__name__)


class Args(NamedTuple):
    run: str
    where: str
    limit_files: int
    all_files: bool
    summary: bool
    files: bool


def show(args: Args):
    run = one_run(args)
    if args.summary:
        _show_summary_and_exit(run)
    if args.files:
        _show_files_and_exit(run)
    with cli.pager():
        _show(run, args)


def Header(run: Run):
    status = run_status(run)
    user_attrs = run_user_attrs(run)
    label = user_attrs.get("label") or ""

    header = Table.grid(
        Column(),
        Column(justify="right"),
        expand=True,
        padding=(0, 1),
        collapse_padding=False,
    )
    header.add_row(
        Text(run.opref.get_full_name(), style="bold " + cli.STYLE_LABEL),
        Text(status, style=cli.run_status_style(status)),
    )
    if label:
        header.add_row(Text(label, style=cli.STYLE_SECOND_LABEL))

    return cli.Panel(header, title=run.id)


def Attributes(run: Run):
    started = run_timestamp(run, "started")
    stopped = run_timestamp(run, "stopped")
    location = format_user_dir(os.path.dirname(run.run_dir))
    project_dir = format_user_dir(run_project_dir(run) or "")
    exit_code = str(run_attr(run, "exit_code", ""))

    attributes = Table.grid(
        Column(style=cli.STYLE_LABEL),
        Column(style=cli.STYLE_VALUE),
        padding=(0, 1),
        collapse_padding=False,
    )
    attributes.add_row("id", run.id)
    attributes.add_row("name", run.name)
    attributes.add_row("started", format_run_timestamp(started))
    attributes.add_row("stopped", format_run_timestamp(stopped))
    attributes.add_row("location", location)
    attributes.add_row("project", project_dir)
    attributes.add_row("exit_code", str(exit_code) if exit_code is not None else "")

    return cli.Panel(attributes, title="Attributes")


def Config(run: Run):
    config = meta_config(run)
    if not config:
        return Group()
    config_table = Table.grid(
        Column(style=cli.STYLE_LABEL),
        Column(style=cli.STYLE_VALUE),
        padding=(0, 1),
        collapse_padding=False,
    )
    for name in sorted(config):
        config_table.add_row(name, str(config[name]))

    return cli.Panel(config_table, title="Configuration")


def Summary(run: Run, table_only: bool = False):
    summary = run_summary(run)
    attributes = summary.get_attributes()
    metrics = summary.get_metrics()
    if not attributes and not metrics:
        return Group()

    table = cli.Table(
        expand=not table_only,
        show_edge=table_only,
        box=_inner_table_box(table_only),
        padding=(0, 1) if table_only else 0,
    )
    table.add_column("name", style=cli.STYLE_LABEL)
    table.add_column("value", style=cli.STYLE_VALUE)
    table.add_column("type", style=cli.STYLE_SUBTEXT)

    for name, attr in sorted(attributes.items()):
        table.add_row(name, format_summary_value(attr), "attribute")
    for name, metric in sorted(metrics.items()):
        table.add_row(name, format_summary_value(metric), "metric")

    if table_only:
        return table
    return cli.Panel(table, title="Summary")


def Files(run: Run, table_only: bool = False, limit: int | None = None):
    with RunManifest(run) as m:
        files = list(m)
    table = cli.Table(
        expand=not table_only,
        show_footer=not table_only,
        show_edge=table_only,
        box=_inner_table_box(table_only),
        padding=(0, 1) if table_only else 0,
    )
    table.add_column(
        "name",
        style=cli.STYLE_VALUE,
        footer_style=f"not b {cli.STYLE_SUBTEXT} i",
    )
    table.add_column(
        "type",
        style=cli.STYLE_SUBTEXT,
    )
    table.add_column(
        "size",
        justify="right",
        style="magenta",
        footer_style="not b magenta i",
    )
    total_count = len(files)
    total_size = 0
    displayed = 0
    for path, type in _sort_files(files):
        stat = os.stat(os.path.join(run.run_dir, path))
        if limit is None or displayed < limit:
            table.add_row(
                path,
                _type_desc(type),
                _format_file_size(stat.st_size),
            )
            displayed += 1
        total_size += stat.st_size
    if displayed < total_count:
        table.add_row("...", "...", "...", style="not b magenta i")
    if not table_only:
        files_desc = "file" if total_count == 1 else "files"
        table.columns[0].footer = (
            f"truncated ({displayed} of {total_count} {files_desc})"
            if displayed < total_count
            else f"{total_count} {files_desc}"
        )
        table.columns[2].footer = f"[i]total: {_format_file_size(total_size)}"

    if table_only:
        return table
    return cli.Panel(table, title="Files")


def _inner_table_box(table_only: bool):
    return (
        None if table_only else rich.box.MARKDOWN if cli.is_plain else rich.box.SIMPLE
    )


def _sort_files(files: list[RunManifestEntry]) -> list[tuple[str, RunFileType]]:
    return [
        (path, type)
        for type_order, path, type in natsorted(
            [(_type_sort_order(type), path, type) for type, digest, path in files]
        )
    ]


def _type_sort_order(type: RunFileType):
    match type:
        case "g":
            return 1
        case "d":
            return 2
        case "s":
            return 3
        case "r":
            return 4
        case _:
            return 5


def _type_desc(type: RunFileType):
    match type:
        case "s":
            return "source code"
        case "d":
            return "dependency"
        case "r":
            return "runtime"
        case "g":
            return "generated"
        case _:
            return f"unknown (type)"


def _format_file_size(size: int):
    if size < 1000:
        # human_readable applies (decimal) formatting to bytes, bypass
        return f"{size} B"
    return human_readable.file_size(size, formatting=".1f")


def Output(run: Run):
    output = list(_iter_run_output(run))
    if not output:
        return Group()
    if len(output) == 1:
        name, reader = output[0]
        return cli.Panel(OutputTable(reader), title="Output")
    return cli.Panel(
        Group(
            *(
                OutputTable(reader, name, pad=i > 0)
                for i, (name, reader) in enumerate(output)
            )
        ),
        title="Output",
    )


def _iter_run_output(run: Run) -> Generator[tuple[str, RunOutputReader], Any, None]:
    output_dirname = run_meta_path(run, "output")
    if not os.path.exists(output_dirname):
        return
    for name in sorted(os.listdir(output_dirname)):
        if os.path.splitext(name)[1] != ".index":
            continue
        name = name[:-6]
        filename = os.path.join(output_dirname, name)
        if not os.path.exists(filename):
            continue
        parts = name.split("_")
        output_name = parts[1] if len(parts) == 2 else parts[0]
        yield output_name, RunOutputReader(filename)


def _output_desc(name: str):
    return {
        "sourcecode": "stage source code",
        "runtime": "stage runtime",
        "dependencies": "stage dependencies",
        "run": "run",
        "finalize": "finalize run",
    }.get(name, name)


def OutputTable(reader: RunOutputReader, name: str = "", pad: bool = False):
    table = cli.Table(
        (name, {"style": "dim"}),
        show_header=name != "",
        expand=True,
        show_edge=False,
        padding=0,
        box=rich.box.SIMPLE_HEAD,
    )
    try:
        lines = list(reader)
    except Exception as e:
        log.warning(
            "Error reading run output (%s): %s",
            reader.filename,
            e,
        )
    else:
        # TODO truncate lines and show message if too long
        for line in lines:
            table.add_row(Text(line.text, style="orange3" if line.stream == 1 else ""))
    return Padding(table, pad=(1 if pad else 0, 0, 0, 0))


def Comments(run: Run):
    comments = get_comments(run)
    if not comments:
        return Group()
    stack = Table.grid(padding=(1, 0))
    for comment in comments:
        stack.add_row(CommentTable(comment))
    return cli.Panel(stack, title="Comments")


def CommentTable(comment: RunComment):
    table = cli.Table(
        show_header=False,
        expand=True,
        show_edge=False,
        padding=0,
        show_lines=True,
        box=rich.box.MARKDOWN if cli.is_plain else rich.box.SIMPLE,
        border_style=cli.STYLE_TABLE_BORDER,
    )
    header = Table.grid(
        Column(style="b " + cli.STYLE_LABEL),
        Column(justify="right", style="bright_black"),
        expand=True,
        padding=(0, 1),
        collapse_padding=False,
    )
    header.add_row(
        comment.author,
        _format_comment_date(comment.timestamp),
    )
    table.add_row(header)
    table.add_row(Markdown(comment.msg))
    return table


def _format_comment_date(timestamp: int):
    comment_date = datetime.datetime.fromtimestamp(timestamp / 1000)
    return human_readable.date_time(comment_date)


def _show(run: Run, args: Args):
    cli.out(Header(run))
    cli.out(Attributes(run))
    cli.out(Config(run))
    cli.out(Summary(run))
    cli.out(Files(run, limit=_files_limit(args)))
    cli.out(Output(run))
    cli.out(Comments(run))


def _files_limit(args: Args):
    return None if args.all_files else args.limit_files


def _show_summary_and_exit(run: Run):
    cli.out(Summary(run, table_only=True))
    raise SystemExit(0)


def _show_files_and_exit(run: Run):
    cli.out(Files(run, table_only=True))
    raise SystemExit(0)
