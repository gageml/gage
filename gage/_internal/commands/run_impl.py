# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import logging
import os
import platform

from .. import cli
from .. import lang
from .. import run_help
from .. import run_sourcecode

from ..run_config import read_project_config
from ..run_context import resolve_run_context
from ..var import runs_dir

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
    _verify_action(args, config, run)
    exit_code = _exec_run(run, args)
    _finalize_run(run, exit_code, args)


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
    config = _run_config(context, args)
    _verify_action(args, config, context)
    run = make_run(context.opref, runs_dir())
    cmd = _op_cmd(context, config)
    user_attrs = _user_attrs(args)
    sys_attrs = _sys_attrs()
    init_run_meta(run, context.opdef, config, cmd, sys_attrs)
    associate_project(run, context.project_dir)
    if user_attrs:
        init_run_user_attrs(run, user_attrs)
    with _RunPhaseStatus(run, args):
        try:
            stage_run(run, context.project_dir)
        except RunExecError as e:
            error_handlers.run_exec_error(e)
    return run


class _RunPhaseStatus:
    def __init__(self, run: Run, args: Args):
        self._phase_desc = {
            "stage-sourcecode": "Staging source code",
            "stage-config": "Applying configuration",
            "stage-runtime": "Staging runtime",
            "stage-dependencies": "Staging dependencies",
            "run": _run_phase_desc(run),
            "finalize": "Finalizing run",
        }

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
            desc = self._phase_desc.get(name)
            if desc:
                self._status.update(f"[dim]{desc}")
            else:
                log.debug("Unexpected run phase callback: %r %r", name, arg)


def _run_phase_desc(run: Run):
    opref = meta_opref(run)
    return f"[dim]Running [{cli.STYLE_PANEL_TITLE}]{opref.op_name}"


def _verify_action(
    args: Args, config: RunConfig, run_or_context: RunContext | Run
) -> None | NoReturn:
    if args.yes:
        return
    cli.out(_action_desc(args, run_or_context), err=True)
    if config:
        cli.out(run_help.config_table(config), err=True)
    else:
        cli.out("", err=True)
    if not cli.confirm(f"Continue?"):
        raise SystemExit(0)


def _action_desc(args: Args, run_or_context: Run | RunContext):
    action = "stage" if args.stage else "run"
    if args.stage:
        assert isinstance(run_or_context, RunContext)
        context = cast(RunContext, run_or_context)
        return f"You are about to stage [yellow]{context.opref.op_name}"
    elif args.start:
        assert isinstance(run_or_context, Run)
        run = cast(Run, run_or_context)
        return f"You are about to start [yellow]{run.opref.op_name}[/] ({run.name})"
    else:
        assert isinstance(run_or_context, RunContext)
        context = cast(RunContext, run_or_context)
        return f"You are about to run [yellow]{context.opref.op_name}"


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


def _run(context: RunContext, args: Args):
    run = _stage(context, args)
    exit_code = _exec_run(run, args)
    _finalize_run(run, exit_code, args)


def _exec_run(run: Run, args: Args):
    with _RunPhaseStatus(run, args):
        try:
            exec_run(run)
        except RunExecError as e:
            return e.exit_code
        else:
            return 0


def _finalize_run(run: Run, exit_code: int, args: Args):
    with _RunPhaseStatus(run, args):
        try:
            finalize_run(run, exit_code)
        except RunExecError as e:
            error_handlers.run_exec_error(e)
        else:
            raise SystemExit(exit_code)
