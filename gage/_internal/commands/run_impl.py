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

from ..run_util import *

from ..run_config import read_project_config
from ..run_context import resolve_run_context
from ..run_output import Progress

from ..var import runs_dir

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
    preview: bool
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
    run = impl_support.one_run_for_spec(args.start)
    status = run_status(run)
    if status != "staged":
        cli.exit_with_error(
            f"Run \"{run.id}\" is '{status}'\n\n"
            "Only staged runs can be started with '--start'."
        )
    config = meta_config(run)
    _verify_action(args, config, run)
    _exec_and_finalize(run, args)


def _apply_default_op_flag_assign(args: Args):
    if not args.opspec:
        return args
    try:
        lang.parse_flag_assign(args.opspec)
    except ValueError:
        return args
    else:
        # opspec is actually a flag assign
        return args._replace(opspec="", flags=[args.opspec] + args.flags)


def _handle_run_context(context: RunContext, args: Args):
    if args.help_op:
        _show_op_help(context, args)
    elif args.preview:
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


class _Status:
    supports_progress = False

    def start(self): ...

    def stop(self): ...

    def update(self, desc: str): ...

    def output(self, output: bytes, progress: Progress | None): ...


class _DefaultStatus(_Status):
    _status = None

    def __init__(self, args: Args):
        if args.quiet or log.getEffectiveLevel() < logging.WARN:
            return
        self._status = cli.status("")

    def start(self):
        if self._status:
            self._status.start()

    def stop(self):
        if self._status:
            self._status.stop()

    def update(self, desc: str):
        if self._status:
            self._status.update(desc)

    def output(self, output: bytes, progress: Progress | None):
        if self._status:
            self._status.console.out(output.decode(), end="")


class _Progress(_Status):
    supports_progress = True
    _progress = None
    _task_id = None

    def __init__(self, args: Args):
        if args.quiet or log.getEffectiveLevel() < logging.WARN:
            return
        self._progress = cli.Progress()
        self._task_id = self._progress.add_task("")

    def start(self):
        if self._progress:
            self._progress.start()

    def stop(self):
        if self._progress:
            self._progress.stop()

    def update(self, desc: str):
        if self._progress:
            assert self._task_id is not None
            self._progress.update(self._task_id, description=desc)

    def output(self, output: bytes, progress: Progress | None):
        if self._progress:
            assert self._task_id is not None
            self._progress.console.out(output.decode(), end="")
            if progress:
                self._progress.update(self._task_id, completed=progress.completed)


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
        self._args = args
        self._status = _DefaultStatus(args)

    def __enter__(self):
        self._status.start()
        run_phase_channel.add(self)

    def __exit__(self, *exc: Any):
        self._status.stop()
        run_phase_channel.remove(self)

    def __call__(self, name: str, arg: Any | None = None):
        if name == "exec-output":
            assert isinstance(arg, tuple), arg
            phase_name, stream, output, progress = arg
            if progress and not self._status.supports_progress:
                # Lazily upgrade status to support progress
                self._status.stop()
                self._status = _Progress(self._args)
                assert self._status.supports_progress
                self._status.start()
            self._status.output(output, progress)
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
    _exec_and_finalize(run, args)


def _exec_and_finalize(run: Run, args: Args):
    with _RunPhaseStatus(run, args):
        exit_code = _exec_run(run, args)
        _finalize_run(run, exit_code, args)


def _exec_run(run: Run, args: Args):
    try:
        exec_run(run)
    except RunExecError as e:
        return e.exit_code
    else:
        return 0


def _finalize_run(run: Run, exit_code: int, args: Args):
    try:
        finalize_run(run, exit_code)
    except RunExecError as e:
        error_handlers.run_exec_error(e)
    else:
        raise SystemExit(exit_code)
