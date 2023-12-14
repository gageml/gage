# SPDX-License-Identifier: Apache-2.0

from typing import *

import logging
import os
import re
import shlex
import time

import click

import rich.box
import rich.columns
import rich.console
import rich.json
import rich.markdown
import rich.markup
import rich.padding
import rich.panel
import rich.progress
import rich.prompt
import rich.status
import rich.style
import rich.text
import rich.theme
import rich.table

import typer.core

__all__ = [
    "AliasGroup",
    "Group",
    "Panel",
    "Progress",
    "Table",
    "confirm",
    "console_width",
    "err",
    "exit_with_error",
    "exit_with_message",
    "incompatible_with",
    "label",
    "markdown",
    "out",
    "status",
    "track",
]

log = logging.getLogger(__name__)

STYLE_METAVAR = "b i cyan"
STYLE_CMD = "b green"

_theme = rich.theme.Theme(
    {
        "arg": STYLE_METAVAR,
        "cmd": STYLE_CMD,
    },
)

_out = rich.console.Console(soft_wrap=False, theme=_theme)

_err = rich.console.Console(stderr=True, soft_wrap=False, theme=_theme)

is_plain = os.getenv("TERM") in ("dumb", "unknown")

STYLE_TABLE_HEADER = "bright_yellow"
STYLE_TABLE_BORDER = "dim"
STYLE_PANEL_TITLE = "yellow"
STYLE_LABEL = "cyan1"
STYLE_SECOND_LABEL = "cyan"
STYLE_VALUE = "dim"


def run_status_style(status: str):
    match status:
        case "completed":
            return "green4"
        case "error" | "terminated":
            return "red3"
        case "staged" | "pending":
            return "bright_black"
        case "running":
            return "yellow italic"
        case _:
            return ""


def console_width():
    return _out.width


def out(
    val: rich.console.RenderableType = "",
    style: str | None = None,
    wrap: bool = False,
    err: bool = False,
):
    print = _err.print if err else _out.print
    print(val, soft_wrap=not wrap, style=style)


def err(val: rich.console.RenderableType = "", style: str | None = None):
    out(val, err=True)


def exit_with_error(msg: rich.console.RenderableType, code: int = 1) -> NoReturn:
    err(msg)
    raise SystemExit(code)


def exit_with_message(msg: rich.console.RenderableType) -> NoReturn:
    err(msg)
    raise SystemExit(0)


def text(s: str, style: str | rich.style.Style = ""):
    return rich.text.Text(s, style=style)


def json(val: Any):
    return rich.json.JSON.from_data(val)


def label(s: str):
    return text(s, style=STYLE_LABEL)


def markdown(md: str):
    return rich.markdown.Markdown(md)


def markup(mu: str):
    return rich.markup.render(mu)


def pad(val: Any, padding: rich.padding.PaddingDimensions):
    return rich.padding.Padding(val, padding)


class YesNoConfirm(rich.prompt.Confirm):
    prompt_suffix = " "

    def make_prompt(self, default: bool) -> rich.text.Text:
        prompt = self.prompt.copy()
        prompt.end = ""
        prompt.append(" ")
        default_part = rich.text.Text(
            "(Y/n)" if default else "(y/N)",
            style="prompt.default",
        )
        prompt.append(default_part)
        prompt.append(self.prompt_suffix)
        return prompt


def confirm(prompt: str, default: bool = True):
    return YesNoConfirm.ask(prompt, default=default, console=_err)


class _NullStatus(rich.status.Status):
    def __init__(self):
        super().__init__("", console=rich.console.Console(quiet=True))

    def start(self):
        pass


def status(description: str = "", quiet: bool = False):
    if quiet or log.getEffectiveLevel() < logging.WARN:
        return _NullStatus()
    return _out.status(description)


def Progress(*cols: str | rich.progress.ProgressColumn, **kw: Any):
    cols = cols or _default_progress_cols()
    return rich.progress.Progress(*cols, **kw)


def _default_progress_cols() -> tuple[rich.progress.ProgressColumn, ...]:
    return (
        rich.progress.TextColumn("[progress.description]{task.description}"),
        rich.progress.BarColumn(),
        rich.progress.TaskProgressColumn(),
        _TimeRemainingColumn(),
    )


class _TimeRemainingColumn(rich.progress.TimeRemainingColumn):
    """Time remaining col with controlled refresh interval.

    The default time remaining column refreshes with the progress bar,
    causing it to change too frequently. Use this class with an optional
    `refresh` arg to control how often the time remaining refreshes.
    """

    def __init__(self, refresh: float = 1.0):
        super().__init__()
        self._refresh = refresh
        self._next_render = 0
        self._last_render = None

    def render(self, task: rich.progress.Task):
        now = time.time()
        if now > self._next_render:
            self._last_render = super().render(task)
            self._next_render = now + self._refresh
        return self._last_render


def track(
    sequence: Sequence[rich.progress.ProgressType]
    | Iterable[rich.progress.ProgressType],
    description: str = "",
    **kw: Any,
):
    return rich.progress.track(sequence, description, **kw)


def incompatible_with(*incompatible: str):
    """Decorator to specify incompatible params."""

    def callback(value: Any, param: typer.core.TyperArgument, ctx: click.Context):
        if not _param_changed(param, value):
            return value
        for used_param in ctx.params:
            if param.name == used_param or used_param not in incompatible:
                continue
            err(
                markup(
                    f"[b cyan]{param.name}[/] and [b cyan]{used_param}[/] "
                    "cannot be used together.\n\n"
                    f"Try '[cmd]{ctx.command_path} {ctx.help_option_names[0]}[/]' "
                    "for help."
                )
            )
            raise SystemExit()
        return value

    return callback


def _param_changed(param: typer.core.TyperArgument, value: Any):
    if param.default is None and value == []:
        # Special case where default is None and value is coerced to an
        # empty list - this is considered unchanged
        return False
    return param.default != value


class pager:
    _pager_env = os.getenv("PAGER") or os.getenv("MANPAGER")

    def __init__(self):
        self._pager = _out.pager(styles=_pager_supports_styles(self._pager_env))

    def __enter__(self):
        if self._pager_env is None:
            os.environ["PAGER"] = "less -r"
        return self._pager.__enter__()

    def __exit__(self, *exc: Any):
        self._pager.__exit__(*exc)
        if self._pager_env is None:
            del os.environ["PAGER"]


def _pager_supports_styles(pager: str | None):
    if pager is None:
        return True
    parts = shlex.split(pager)
    return parts[0] == "less" and "-r" in parts[1:]


ColSpec = str | tuple[str, dict[str, Any]]


def Table(*cols: ColSpec, **kw: Any):
    box = kw.pop("box", None) or (rich.box.MARKDOWN if is_plain else rich.box.ROUNDED)
    border_style = kw.pop("border_style", None) or STYLE_TABLE_BORDER
    header_style = kw.pop("header_style", None) or STYLE_TABLE_HEADER
    t = rich.table.Table(
        box=box,
        border_style=border_style,
        header_style=header_style,
        **kw,
    )
    for col in cols:
        col_header, col_kw = _split_col(col)
        t.add_column(col_header, **col_kw)
    return t


def _split_col(col: ColSpec) -> tuple[str, dict[str, Any]]:
    if isinstance(col, str):
        return col, {}
    else:
        header, kw = col
        return header, kw


def Panel(renderable: rich.console.RenderableType, **kw: Any):
    title = kw.pop("title", None)
    if isinstance(title, str):
        title = rich.text.Text(title, STYLE_PANEL_TITLE)
    return rich.panel.Panel(
        renderable,
        title=title,
        box=rich.box.ROUNDED if not is_plain else rich.box.MARKDOWN,
        **kw,
    )


def Group(*renderables: rich.console.RenderableType):
    return rich.console.Group(*renderables)


def Columns(renderables: Iterable[rich.console.RenderableType], **kw: Any):
    return rich.columns.Columns(renderables, **kw)


class AliasGroup(typer.core.TyperGroup):
    """click Group subclass that supports commands with aliases.

    To alias a command, include the aliases in the command name,
    separated by commas.
    """

    _CMD_SPLIT_P = re.compile(r", ?")

    def get_command(self, ctx: click.Context, cmd_name: str):
        return super().get_command(ctx, self._map_name(cmd_name))

    def _map_name(self, default_name: str):
        for cmd in self.commands.values():
            if cmd.name and default_name in self._CMD_SPLIT_P.split(cmd.name):
                return cmd.name
        return default_name


def rich_markdown_element(name: str):
    def decorator(cls: Type[rich.markdown.MarkdownElement]):
        rich.markdown.Markdown.elements[name] = cls

    return decorator


@rich_markdown_element("heading_open")
class _MarkdownHeading(rich.markdown.Heading):
    """Internal Rich Markdown heading.

    To change the hard-coded heading alignment from center to left, we
    need to patch `rich.markdown.Markdown.elements` with a modified
    heading implementation.
    """

    def __rich_console__(
        self,
        console: rich.console.Console,
        options: rich.console.ConsoleOptions,
    ) -> rich.console.RenderResult:
        text = self.text
        if self.tag == "h1":
            text.justify = "center"
            yield rich.panel.Panel(text, style="dim")
        else:
            yield text


@rich_markdown_element("table_open")
class _TableElement(rich.markdown.TableElement):
    def __rich_console__(
        self,
        console: rich.console.Console,
        options: rich.console.ConsoleOptions,
    ) -> rich.console.RenderResult:
        table = rich.table.Table(box=rich.box.ROUNDED)

        if self.header is not None and self.header.row is not None:
            for column in self.header.row.cells:
                table.add_column(column.content)

        if self.body is not None:
            for row in self.body.rows:
                row_content = [element.content for element in row.cells]
                table.add_row(*row_content)

        yield table


class PlainHelpFormatter(click.HelpFormatter):
    def write_text(self, text: str) -> None:
        super().write_text(_strip_markup(text))

    def write_dl(
        self,
        rows: Sequence[Tuple[str, str]],
        col_max: int = 30,
        col_spacing: int = 2,
    ) -> None:
        super().write_dl(
            [(name, _strip_markup(val)) for name, val in rows],
            col_max,
            col_spacing,
        )


def _strip_markup(s: str):
    return rich.markup.render(s).plain
