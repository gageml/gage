# SPDX-License-Identifier: Apache-2.0

from typing import *

from gage._internal import exitcodes

from ..types import *

import logging
import os
import platform
import sys

from .. import cli
from .. import lang
from .. import run_help
from .. import run_sourcecode

from ..run_util import *

from ..run_attr import run_config
from ..run_attr import run_opref
from ..run_attr import run_status

from ..run_config_util import read_project_config
from ..run_context import resolve_run_context
from ..run_output import Progress
from ..run_select import find_comparable_run

from ..var import runs_dir

from . import error_handlers
from . import impl_support

log = logging.getLogger(__name__)


class Skipped(Exception):
    def __init__(self, comparable_run: Run):
        self.comparable_run = comparable_run


class Args(NamedTuple):
    opspec: str
    flags: list[str]
    label: str
    stage: bool
    start: str | None
    needed: bool
    batch: list[str]
    max_runs: int
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
        _handle_run(args)


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


def _handle_start(args: Args):
    assert args.start
    run = impl_support.one_run_for_spec(args.start)
    status = run_status(run)
    if status != "staged":
        cli.exit_with_error(
            f"Run \"{run.id}\" is '{status}'\n\n"
            "Only staged runs can be started with '--start'."
        )
    config = run_config(run)
    _verify_run_or_stage(args, config, run)
    _exec_and_finalize(run, args)


def _handle_run(args: Args):
    try:
        context = resolve_run_context(args.opspec)
    except FileNotFoundError as e:
        error_handlers.gagefile_not_found(e)
    except GageFileError as e:
        error_handlers.gagefile_error(e)
    except OpDefNotFound as e:
        error_handlers.opdef_not_found(e)
    else:
        _handle_run_context(context, args)


def _handle_run_context(context: RunContext, args: Args):
    if args.help_op:
        _show_op_help(context, args)
    elif args.batch:
        _handle_batch(context, args)
    elif args.preview:
        _preview(context, args)
    elif args.stage:
        _handle_stage(context, args)
    else:
        _run(context, args)


def _handle_batch(context: RunContext, args: Args):
    from . import batch_impl

    batch_impl.handle_run_context(context, args)


def _handle_stage(context: RunContext, args: Args):
    try:
        run = _stage(context, args)
    except Skipped as e:
        _handle_skipped(e)
    else:
        cli.out(
            f"Run \"{run.name}\" is staged\n\n"
            f"To start it, run '[cmd]gage run --start {run.name}[/]'"
        )


def _handle_skipped(skipped: Skipped):
    run_name = skipped.comparable_run.name
    cli.err(
        f"Run skipped because a comparable run exists ([cyan]{run_name})[/]\n\n"
        f"For details, run '[cmd]gage show {run_name}[/]'"
    )
    raise SystemExit(exitcodes.SKIPPED)


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


class _Status:
    supports_progress = False

    def start(self): ...

    def stop(self): ...

    def update(self, desc: str): ...

    def output(self, output: bytes, progress: Progress | None): ...


class _DefaultStatus(_Status):
    def __init__(self, args: Args):
        self._quiet = args.quiet
        self._status = (
            cli.status("")
            if not args.quiet and not log.getEffectiveLevel() < logging.WARN
            else None
        )

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
            output_str = output.decode(errors="replace")
            self._status.console.out(output_str, end="")
        elif not self._quiet:
            sys.stdout.buffer.write(output)


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


class _RunPhaseContextManager:

    def __enter__(self): ...

    def __exit__(self, *exc: Any) -> '_RunPhaseContextManager': ...


_RUN_PHASE_DESC = {
    "stage-sourcecode": "[dim]Staging source code[/]",
    "stage-config": "[dim]Applying configuration[/]",
    "stage-runtime": "[dim]Staging runtime[/]",
    "stage-dependencies": "[dim]Staging dependencies[/]",
    "run": f"[dim]Running [{cli.STYLE_PANEL_TITLE}]{{op_name}}[/]",
    "finalize": "[dim]Finalizing run[/]",
}


class _RunPhaseStatus(_RunPhaseContextManager):
    def __init__(self, run: Run, args: Args):
        self._args = args
        self._status = _DefaultStatus(args)
        self._run_attrs = {"op_name": run_opref(run).op_name}

    def __enter__(self):
        self._status.start()
        run_phase_channel.add(self)
        return self

    def __exit__(self, *exc: Any):
        self._status.stop()
        run_phase_channel.remove(self)

    def __call__(self, name: str, arg: Any | None = None):
        if name == "exec-output":
            assert isinstance(arg, tuple), arg
            run, phase_name, stream, output, progress = arg
            if progress and not self._status.supports_progress:
                # Lazily upgrade status to support progress
                self._status.stop()
                self._status = _Progress(self._args)
                assert self._status.supports_progress
                self._status.start()
            self._status.output(output, progress)
        else:
            desc = _RUN_PHASE_DESC.get(name)
            if desc:
                try:
                    formatted = desc.format(**self._run_attrs)
                except KeyError as e:
                    log.debug(
                        "Run phase desc format error for %r: missing %r",
                        desc,
                        e.args[0],
                    )
                    formatted = desc
                self._status.update(formatted)
            else:
                log.debug("Unexpected run phase callback: %r %r", name, arg)


def _stage(
    context: RunContext,
    args: Args,
    config: RunConfig | None = None,
    run_phase_status: _RunPhaseContextManager | None = None,
):
    config = config or _run_config(context, args)
    _verify_run_or_stage(args, config, context)
    _check_needed(args, config, context)
    run = make_run(context.opref, runs_dir())
    run_phase_status = run_phase_status or _RunPhaseStatus(run, args)
    cmd = _op_cmd(context, config)
    user_attrs = _user_attrs(args)
    sys_attrs = _sys_attrs()
    init_run_meta(run, context.opdef, config, cmd, sys_attrs)
    associate_project(run, context.project_dir)
    if user_attrs:
        init_run_user_attrs(run, user_attrs)
    with run_phase_status:
        try:
            stage_run(run, context.project_dir)
        except RunExecError as e:
            error_handlers.run_exec_error(e)
    return run


def _check_needed(args: Args, config: RunConfig, context: RunContext):
    if not args.needed:
        return
    run = find_comparable_run(context.opref, config)
    if run:
        raise Skipped(run)


def _verify_run_or_stage(
    args: Args,
    config: RunConfig,
    run_or_context: RunContext | Run,
) -> None | NoReturn:
    if args.yes:
        return
    cli.err(_action_desc(args, run_or_context))
    if config:
        cli.err(run_help.config_table(config))
    else:
        cli.err("")
    if not cli.confirm(f"Continue?"):
        raise SystemExit(0)


def _action_desc(args: Args, run_or_context: Run | RunContext):
    if args.stage:
        assert isinstance(run_or_context, RunContext)
        context = cast(RunContext, run_or_context)
        return f"You are about to stage [yellow]{context.opref.op_name}[/]"
    elif args.start:
        assert isinstance(run_or_context, Run)
        run = cast(Run, run_or_context)
        return f"You are about to start [yellow]{run.opref.op_name}[/] ({run.name})"
    else:
        assert isinstance(run_or_context, RunContext)
        context = cast(RunContext, run_or_context)
        return f"You are about to run [yellow]{context.opref.op_name}[/]"


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
    try:
        run = _stage(context, args)
    except Skipped as e:
        _handle_skipped(e)
    else:
        _exec_and_finalize(run, args)


def _exec_and_finalize(
    run: Run,
    args: Args,
    run_phase_status: _RunPhaseContextManager | None = None,
):
    run_phase_status = run_phase_status or _RunPhaseStatus(run, args)
    with run_phase_status:
        try:
            exit_code = _exec_run(run, args)
        except KeyboardInterrupt:
            _finalize_run(run, -2, args)
            raise
        else:
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
