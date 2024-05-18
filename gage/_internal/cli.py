# SPDX-License-Identifier: Apache-2.0

from typing import *

import logging
import os
import re
import shlex
import sys
import time

import click
from click.core import ParameterSource

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

from typer import Context
from typer import Option as OptionBase
from typer import Argument as ArgumentBase
from typer.core import TyperGroup
from typer.core import TyperArgument

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
    "format_assigns",
    "label",
    "markdown",
    "out",
    "status",
    "track",
    "truncate",
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
STYLE_SUBTEXT = "bright_black"


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


def exit_with_error(
    msg: rich.console.RenderableType, code: str | int | None = 1
) -> NoReturn:
    raise SystemExit(msg, code)


def exit_with_message(msg: rich.console.RenderableType) -> NoReturn:
    raise SystemExit(msg, 0)


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


def Progress(
    *cols: str | rich.progress.ProgressColumn,
    transient: bool = True,
    **kw: Any,
):
    cols = cols or _default_progress_cols()
    return rich.progress.Progress(*cols, transient=transient, **kw)


def _default_progress_cols() -> tuple[rich.progress.ProgressColumn, ...]:
    return (
        rich.progress.TextColumn("[progress.description]{task.description}"),
        rich.progress.BarColumn(),
        rich.progress.TaskProgressColumn(),
        _TimeRemainingColumn(),
    )


Status = rich.status.Status


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
    sequence: (
        Sequence[rich.progress.ProgressType] | Iterable[rich.progress.ProgressType]
    ),
    description: str = "",
    **kw: Any,
):
    return rich.progress.track(sequence, description, **kw)


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


def Table(*cols: ColSpec, width: int | None = None, **kw: Any):
    box = kw.pop("box", None) or (rich.box.MARKDOWN if is_plain else rich.box.ROUNDED)
    border_style = kw.pop("border_style", None) or STYLE_TABLE_BORDER
    header_style = kw.pop("header_style", None) or STYLE_TABLE_HEADER
    t = rich.table.Table(
        box=box,
        border_style=border_style,
        header_style=header_style,
        width=_table_width_windows_markdown(width, box),
        **kw,
    )
    for col in cols:
        col_header, col_kw = _split_col(col)
        t.add_column(col_header, **col_kw)
    return t


def _table_width_windows_markdown(table_width_arg: int | None, box: rich.box.Box):
    if (
        table_width_arg is None
        and sys.platform == "win32"
        and box is rich.box.MARKDOWN
        and "COLUMNS" in os.environ
    ):
        # On Windows when using markdown formatted tables, width is one
        # less than expected when COLUMNS is used to infer width (i.e.
        # when width is None). When `width` is provide to a table
        # explicitly, the generated output is as expected.
        return int(os.environ["COLUMNS"])
    return table_width_arg


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


class AliasGroup(TyperGroup):
    """click Group subclass that supports commands with aliases.

    To alias a command, include the aliases in the command name,
    separated by commas.
    """

    _CMD_SPLIT_P = re.compile(r", ?")

    def get_command(self, ctx: Context, cmd_name: str):
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


Assign = tuple[str, str]

AssignStyles = tuple[str, str, str]

MIN_ASSIGN_WIDTH = 11
MIN_ASSIGN_NAME = 4


def format_assigns(
    assigns: list[Assign],
    available_width: int,
    styles: AssignStyles | None = None,
    min_assign_width: int = MIN_ASSIGN_WIDTH,
) -> str:
    assigns = _fit_assigns(assigns, available_width, min_assign_width)
    parts = []
    while assigns:
        avg_budget = available_width // len(assigns)
        assign_budget = avg_budget + _lookahead_unused(assigns[1:], avg_budget)
        name, val = assigns.pop(0)
        assign_wants = len(name) + len(val) + 1
        if parts and available_width < min(assign_wants, min_assign_width):
            break
        fit_name, fit_val = _fit_assign(name, val, assign_budget)
        available_width -= len(fit_name) + len(fit_val) + 2 if assigns else 1
        parts.append(_format_assign(fit_name, fit_val, styles))
    return " ".join(parts)


def _lookahead_unused(assigns: list[Assign], avg_budget: int):
    unused_total = 0
    allocation_targets = 1
    for name, val in assigns:
        needed = len(name) + len(val) + 1
        if needed < avg_budget:
            unused_total += avg_budget - needed
        else:
            allocation_targets += 1
    return unused_total // allocation_targets


def _fit_assigns(
    assigns: list[Assign],
    total_width: int,
    min_assign_width: int = MIN_ASSIGN_WIDTH,
) -> list[Assign]:
    fit: list[Assign] = []
    running_width = 0
    for assign in assigns:
        assign_wants = len(assign[0]) + len(assign[1]) + 1
        running_width += min(assign_wants, min_assign_width) + (1 if fit else 0)
        if fit and running_width > total_width:  # Include at least one assign
            break
        fit.append(assign)
    return fit


DEBUG = 0


def _fit_assign(name: str, val: str, budget: int, min_name: int = 4, min_val: int = 4):
    len_name = len(name)
    len_val = len(val)
    fit_name = min(len_name, min_name)
    fit_val = min(len_val, min_val)
    available = max(0, budget - fit_name - fit_val - 1)
    if DEBUG:
        print("#########", available, budget, fit_name, fit_val)
    while available:
        applied = False
        if DEBUG:
            print(
                f"### check for name: {fit_name} < {len_name} and ({fit_name} <= {fit_val} or {fit_val} >= {len_val}"
            )
        if fit_name < len_name and (fit_name <= fit_val or fit_val >= len_val):
            fit_name += 1
            available -= 1
            applied = True
            if DEBUG:
                print("#### applied to name")
        if DEBUG:
            print(
                f"### check for val: {available} and {fit_val} < {len_val} and ({fit_val} <= {fit_name} or {fit_name} >= {len_name}"
            )
        if (
            available
            and fit_val < len_val
            and (fit_val <= fit_name or fit_name >= len_name)
        ):
            fit_val += 1
            available -= 1
            applied = True
            if DEBUG:
                print("#### applied to val")
        if not applied:
            if DEBUG:
                print("#### did not apply")
            break

    return (
        truncate(name, fit_name),
        truncate(val, fit_val),
    )


def truncate(s: str, to_len: int):
    return f"{s[:to_len-1]}…" if to_len < len(s) else s


def _format_assign(name: str, val: str, styles: AssignStyles | None):
    name_style = styles[0] if styles else ""
    eq_style = styles[1] if styles else ""
    val_style = styles[2] if styles else ""
    name = f"[{name_style}]{name}[/{name_style}]" if name_style else name
    eq = f"[{eq_style}]=[/{eq_style}]" if eq_style else "="
    val = f"[{val_style}]{val}[/{val_style}]" if val_style else val
    return name + eq + val


def Argument(
    default: Any | None = ...,
    *,
    metavar: str | None = None,
    show_default: bool | str = True,
    help: str | None = None,
    incompatible_with: list[str] | None = None,
):
    return ArgumentBase(
        default,
        metavar=metavar,
        show_default=show_default,
        help=help,
        callback=_option_callback(incompatible_with),
    )


def Option(
    default: Any | None = ...,
    *param_decls: str,
    metavar: str | None = None,
    show_default: bool = True,
    is_flag: bool | None = None,
    help: str | None = None,
    hidden: bool = False,
    incompatible_with: list[str] | None = None,
):
    return OptionBase(
        default,
        *param_decls,
        metavar=metavar,
        show_default=show_default,
        is_flag=is_flag,
        help=help,
        hidden=hidden,
        callback=_option_callback(incompatible_with),
    )


def _option_callback(incompatible: list[str] | None):
    def check_compatible(value: Any, param: TyperArgument, ctx: Context):
        if not param.name or _is_default_param_value(param, value, ctx):
            return param.type_cast_value(ctx, value)
        if incompatible:
            _add_implied_incompatible(param.name, incompatible, ctx)
        real_incompatible = incompatible or _implied_incompatible(param.name, ctx)
        if not real_incompatible:
            return param.type_cast_value(ctx, value)
        for used_param in ctx.params:
            if param.name == used_param or used_param not in real_incompatible:
                continue
            err(
                markup(
                    f"[b cyan]{used_param}[/] and [b cyan]{param.name}[/] "
                    "cannot be used together.\n\n"
                    f"Try '[cmd]{ctx.command_path} {ctx.help_option_names[0]}[/]' "
                    "for help."
                )
            )
            raise SystemExit()
        return param.type_cast_value(ctx, value)

    return check_compatible


def _is_default_param_value(param: TyperArgument, value: Any, ctx: Context):
    if param.default == value or param.default is None and value == []:
        # The check for value == [] here accommodates the case where a
        # param value is coerced to an empty list - we consider this a
        # default value as nothing has been provided on the command
        # line.
        return True
    source = ctx.get_parameter_source(param.name or "")
    return source is ParameterSource.DEFAULT


def _add_implied_incompatible(param_name: str, incompatible: list[str], ctx: Context):
    implied = ctx.meta.setdefault("incompatible", {})
    for name in incompatible:
        implied.setdefault(name, []).append(param_name)


def _implied_incompatible(param_name: str, ctx: Context):
    return ctx.meta.get("incompatible", {}).get(param_name)
