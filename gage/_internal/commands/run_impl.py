# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import logging
import os
import platform

from rich.console import Console

from .. import cli
from .. import lang
from .. import run_help
from .. import run_output
from .. import run_sourcecode

from ..sys_config import get_runs_home
from ..run_config import read_project_config
from ..run_context import resolve_run_context

from ..run_util import *

from . import error_handlers
from . import impl_support

log = logging.getLogger(__name__)


class Args(NamedTuple):
    opspec: str
    flags: list[str]
    label: str
    stage: bool
    start: str | None
    quiet: bool
    yes: bool
    help_op: bool
    preview_sourcecode: bool
    preview_all: bool
    json: bool


def run(args: Args):
    args = _apply_default_op_flag_assign(args)
    if args.start:
        _handle_start(args)
    else:
        try:
            context = resolve_run_context(args.opspec)
        except GageFileError as e:
            error_handlers.gagefile_error(e)
        except OpDefNotFound as e:
            error_handlers.opdef_not_found(e)
        else:
            _handle_run_context(context, args)


def _handle_start(args: Args):
    assert args.start
    run = impl_support.one_run(args.start)
    status = run_status(run)
    if status != "staged":
        cli.exit_with_error(
            f"Run \"{run.id}\" is '{status}'\n\n"
            "Only staged runs can be started with '--start'."
        )
    config = meta_config(run)
    _verify_action(args, run, config)
    _run_staged(run, args)


def _apply_default_op_flag_assign(args: Args):
    if not args.opspec:
        return args
    try:
        lang.parse_flag_assign(args.opspec)
    except ValueError:
        return args
    else:
        # opspec is actually a flag assign
        return Args("", [args.opspec] + args.flags, *args[2:])


def _handle_run_context(context: RunContext, args: Args):
    if args.help_op:
        _show_op_help(context, args)
    elif _preview_opts(args):
        _preview(context, args)
    elif args.stage:
        _handle_stage(context, args)
    else:
        _run(context, args)


def _handle_stage(context: RunContext, args: Args):
    run = _stage(context, args)
    cli.out(
        f"Run \"{run.name}\" is staged\n\n"
        f"To start it, run '[cmd]gage run --start {run.name}[/]'"
    )


# =================================================================
# Op help
# =================================================================


def _show_op_help(context: RunContext, args: Args):
    cli.out(run_help.get_help(args.opspec, context))


# =================================================================
# Preview
# =================================================================


def _preview_opts(args: Args):
    return args.preview_sourcecode or args.preview_all


def _preview(context: RunContext, args: Args):
    previews = _init_previews(context.opdef, args)
    if args.json:
        cli.out(
            cli.json({name: as_json() for name, as_renderable, as_json in previews})
        )
    else:
        cli.out(
            cli.Group(*(as_renderable() for name, as_renderable, as_json in previews))
        )


class Preview(NamedTuple):
    name: str
    as_renderable: Callable[[], Any]
    as_json: Callable[[], Any]


def _init_previews(opdef: OpDef, args: Args):
    previews: list[Preview] = []
    if args.preview_sourcecode or args.preview_all:
        previews.append(_init_sourcecode_preview(opdef))
    # TODO: other previews
    return previews


def _init_sourcecode_preview(opdef: OpDef):
    project_dir = os.path.dirname(opdef.get_src())
    sourcecode = run_sourcecode.init(project_dir, opdef)
    return Preview(
        "sourcecode",
        lambda: run_sourcecode.preview(sourcecode),
        lambda: sourcecode.as_json(),
    )


# =================================================================
# Stage and run
# =================================================================


def _stage(context: RunContext, args: Args):
    run = make_run(context.opref, get_runs_home())
    config = _run_config(context, args)
    _verify_action(args, run, config)
    cmd = _op_cmd(context, config)
    user_attrs = _user_attrs(args)
    sys_attrs = _sys_attrs()
    init_run_meta(run, context.opdef, config, cmd, sys_attrs)
    associate_project(run, context.project_dir)
    if user_attrs:
        init_run_user_attrs(run, user_attrs)
    with _RunPhaseStatus(args):
        stage_run(run, context.project_dir)
    return run


class _RunPhaseStatus:
    phase_desc = {
        "stage-sourcecode": "Staging source code",
        "stage-config": "Applying configuration",
        "stage-runtime": "Staging runtime",
        "stage-dependencies": "Staging dependencies",
        "finalize": "Finalizing run",
    }

    def __init__(self, args: Args):
        self._status = cli.status("", args.quiet)

    def __enter__(self):
        self._status.start()
        run_phase_channel.add(self)

    def __exit__(self, *exc: Any):
        self._status.stop()
        run_phase_channel.remove(self)

    def __call__(self, name: str, arg: Any | None = None):
        if name == "exec-output":
            assert isinstance(arg, tuple), arg
            phase_name, stream, output = arg
            self._status.console.out(cast(bytes, output).decode(), end="")
        else:
            desc = self.phase_desc.get(name)
            if desc:
                self._status.update(f"[dim]{desc}")
            else:
                log.debug("Unexpected run phase callback: %r %r", name, arg)


def _verify_action(args: Args, run: Run, config: RunConfig) -> None | NoReturn:
    if args.yes:
        return
    action = "stage" if args.stage else "run"
    cli.err(f"You are about to {action} [yellow]{run.opref.get_full_name()}[/]")
    if config:
        cli.err(run_help.config_table(config))
    else:
        cli.err("")
    if not cli.confirm(f"Continue?"):
        raise SystemExit(0)


def _run_config(context: RunContext, args: Args):
    project_config = read_project_config(context.project_dir, context.opdef)
    flags_config = _parse_flags_config(args)
    return cast(RunConfig, {**project_config, **flags_config})


def _parse_flags_config(args: Args):
    return cast(RunConfig, dict([_parse_flag_assign(flag) for flag in args.flags]))


def _parse_flag_assign(flag_assign: str) -> tuple[str, RunConfigValue]:
    try:
        return lang.parse_flag_assign(flag_assign)
    except ValueError as e:
        cli.exit_with_error(f"Invalid flag assignment: {e}")


def _op_cmd(context: RunContext, config: RunConfig):
    cmd_args = context.opdef.get_exec().get_run()
    if not cmd_args:
        error_handlers.missing_exec_error(context)
    resolved_cmd_args = _resolve_cmd_args(cmd_args, context)
    env = {}
    return OpCmd(resolved_cmd_args, env)


def _resolve_cmd_args(cmd_args: CmdArgs, context: RunContext):
    if isinstance(cmd_args, str):
        return _resolve_project_dir_env(cmd_args, context.project_dir)
    return [_resolve_project_dir_env(arg, context.project_dir) for arg in cmd_args]


def _resolve_project_dir_env(arg: str, project_dir: str):
    return arg.replace("$project_dir", project_dir)


def _user_attrs(args: Args):
    attrs: dict[str, Any] = {}
    if args.label:
        attrs["label"] = args.label
    return attrs


def _sys_attrs():
    return {"platform": platform.platform()}


class _OutputCallback(run_output.OutputCallback):
    def __init__(self, console: Console):
        self.console = console

    def output(self, stream: run_output.StreamType, out: bytes):
        self.console.out(out.decode(), end="")

    def close(self):
        pass


def _run(context: RunContext, args: Args):
    run = _stage(context, args)
    _run_staged(run, args)


def _run_staged(run: Run, args: Args):
    status = cli.status(_running_status_desc(run), args.quiet)
    output_cb = _OutputCallback(status.console)
    with status:
        proc = start_run(run)
        output = open_run_output(run, proc, output_cb=output_cb)
        exit_code = proc.wait()
        output.wait_and_close()
    with _RunPhaseStatus(args):
        finalize_run(run, exit_code)


def _running_status_desc(run: Run):
    opref = meta_opref(run)
    return f"[dim]Running [{cli.STYLE_PANEL_TITLE}]{opref.get_full_name()}"
