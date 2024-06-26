# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import json
import logging
import os
import re
import subprocess
import time

from .. import cli

from ..run_attr import run_user_dir
from ..file_util import make_temp_dir
from ..util import flatten
from ..util import which
from ..var import runs_dir

from .impl_support import runs_table
from .impl_support import selected_runs

log = logging.getLogger(__name__)


class _CopyError(Exception):
    def __init__(self, exit_code: int, out: str):
        self.exit_code = exit_code
        self.out = out


class Args(NamedTuple):
    runs: list[str]
    dest: str
    src: str
    where: str
    all: bool
    yes: bool
    sync: bool
    verbose: int


def copy(args: Args):
    if not args.runs and not args.all:
        cli.exit_with_error(
            "Specify a run to copy or use '--all'.\n\n"
            "Use '[cmd]gage list[/]' to show available runs.\n\n"
            f"Try '[cmd]gage copy -h[/]' for additional help."
        )
    if args.dest:
        _copy_to(args)
    elif args.src:
        _copy_from(args)
    else:
        cli.exit_with_error(
            "Option '--src' or '--dest' is required.\n\n"
            f"Try '[cmd]gage copy -h[/]' for additional help."
        )


def _copy_to(args: Args):
    runs = _selected_runs(args)
    _user_confirm_copy_to(args, runs)
    _copy_to_([run for index, run in runs], args.dest, args.sync, args.verbose)


def _copy_to_(
    runs: list[Run],
    dest: str,
    sync: bool = False,
    verbose: int = 0,
    config_path: str | None = None,
    env: dict[str, str] | None = None,
    no_summary: bool = False,
):
    pre_copy_status = cli.status("Preparing copy")
    pre_copy_status.start()
    src_dir, includes, total_bytes = _prepare_copy(runs)
    p = None  # Lazy init to avoid progress display on early error
    task = None
    nothing_copied = False
    try:
        for total_copied, output in _rclone_copy_to(
            src_dir,
            dest,
            includes,
            sync,
            verbose,
            config_path,
            env,
        ):
            if total_copied == -1:
                nothing_copied = True
                break
            pre_copy_status.stop()
            if p is None:
                p = cli.Progress()
                p.start()
                task = p.add_task("Copying runs", total=total_bytes)
            assert task is not None
            if output:
                p.console.out(output)
            if total_copied:
                p.update(task, completed=total_copied)
    except _CopyError as e:
        _handle_copy_error(e)
    finally:
        pre_copy_status.stop()
        if p:
            p.stop()

    runs_count = "1 run" if len(runs) == 1 else f"{len(runs)} runs"
    if not no_summary:
        if nothing_copied:
            cli.err("Nothing copied, runs are up-to-date")
        else:
            cli.err(f"Copied {runs_count}")


def _prepare_copy(runs: list[Run]):
    src_dir, includes = _src_run_includes(runs)
    total_bytes = _rclone_size(src_dir, includes)
    return src_dir, includes, total_bytes


def _handle_copy_error(e: _CopyError):
    m = re.search(
        r"Failed to create file system for \"(.+?):\": didn't "
        r"find section in config file",
        e.out,
    )
    if m:
        cli.exit_with_error(
            f"Undefined remote \"{m.group(1)}\"\n\n"
            "Try '[cmd]gage help remotes[/]' for help defining and using remotes."
        )
    else:
        log.error(e.out)
        cli.exit_with_error("Error copying runs - see output above for details")


def _selected_runs(args: Args):
    runs, from_count = selected_runs(args)
    if not runs:
        cli.exit_with_error("Nothing selected")
    return runs


def _user_confirm_copy_to(args: Args, runs: list[IndexedRun]):
    if args.yes:
        return
    table = runs_table(runs)
    cli.out(table)
    run_count = "1 run" if len(runs) == 1 else f"{len(runs)} runs"
    cli.err(f"You are about copy {run_count} to {args.dest}")
    cli.err()
    if not cli.confirm(f"Continue?"):
        raise SystemExit(0)


def _src_run_includes(runs: list[Run]):
    if not runs:
        return _empty_src_run_includes()
    src_root = None
    includes: list[str] = []
    for run in runs:
        for src in _run_sources(run):
            src_parent, name = os.path.split(src)
            if not src_root:
                src_root = src_parent
            assert src_parent == src_root, (src, src_parent)
            if os.path.isdir(src):
                includes.append(f"/{name}/**")
            else:
                includes.append(f"/{name}")
    assert src_root
    return src_root, includes


def _empty_src_run_includes() -> tuple[str, list[str]]:
    tmp = make_temp_dir("gage-empty-")
    return tmp, []


def _run_sources(run: Run):
    if not os.path.exists(run.meta_dir):
        return
    yield run.meta_dir
    if os.path.exists(run.run_dir):
        yield run.run_dir
    user_dir = run_user_dir(run)
    if os.path.exists(user_dir):
        yield user_dir


def _rclone_size(src: str, includes: list[str]):
    p = subprocess.Popen(
        _rclone_cmd(["size", src, "--include-from", "-", "--json"]),
        text=True,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert p.stdin
    assert p.stdout
    assert p.stderr
    for include in includes:
        p.stdin.write(include + "\n")
    p.stdin.close()
    result = p.wait()
    out = p.stdout.read()
    if result != 0:
        err = p.stderr.read()
        raise RuntimeError(result, err)
    return _parse_rclone_size(out)


def _rclone_cmd(args: list[str]):
    exe = which("rclone")
    if not exe:
        raise SystemExit(
            "rclone is required for this command\n"
            "Refer to https://www.gage.chat/rclone/ for help."
        )
    return [exe, *args]


def _parse_rclone_size(out: str):
    try:
        data = json.loads(out)
    except json.decoder.JSONDecodeError as e:
        raise AssertionError(e, out)
    else:
        bytes = data.get("bytes")
        assert isinstance(bytes, int), data
        return bytes


_TRANSFERRED_P = re.compile(r"([\d\.]+) ([\S]+) /")


def _rclone_copy_to(
    src: str,
    dest: str,
    includes: list[str],
    sync: bool = False,
    verbose: int = 0,
    config_path: str | None = None,
    env: dict[str, str] | None = None,
):
    """Use rclone to copy to a location.

    `includes` is a list of patterns under `src` to include. Anything is
    skipped.

    Yields a tuple of bytes copied and output line. Either value may be
    None.

    `verbose` is a number indicating the level of output verbosity.
    Output based on this argument is provided by the generator as the
    second item in the tuple.

    Implementation notes:

    - Always use at least one level of verbosity to cause progress
      reports to be printed at the expected interval. This is required
      due to rclone's surprising behavior of not printing progress when
      `--stats-one-line` is specified.

    - Set `--stats-log-level` to DEBUG to avoid duplicate progress
      output. Progress output is shown per line when at least one `-v`
      option is used.

    - Stats are configured to print ever 100ms. This seems fast but is
      the frequency needed for smooth progress bars. Note that progress
      output is never yielded as output so this frequency does not
      impact performance even with verbose logging.

    """
    rclone_cmd = "sync" if sync else "copy"
    config_opts = ["--config", config_path] if config_path else []
    sync_opts = ["--delete-excluded"] if sync else []
    verbose_opts = ["-" + "v" * verbose] if verbose else ["-v"]
    env = {**os.environ, **(env if env else {})}
    cmd = _rclone_cmd(
        [
            rclone_cmd,
            src,
            dest,
            "--include-from",
            "-",
            "--progress",
            "--stats",
            "100ms",
            "--stats-one-line",
            "--stats-log-level",
            "DEBUG",
            # Options to improve performance (ruled out --no-traverse)
            "--size-only",
        ]
        + config_opts
        + sync_opts
        + verbose_opts
    )
    log.debug("Copy cmd: %s", cmd)
    p = subprocess.Popen(
        cmd,
        env=env,
        text=True,
        stdin=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        stdout=subprocess.PIPE,
    )
    assert p.stdin
    assert p.stdout
    for include in includes:
        p.stdin.write(include + "\n")
    p.stdin.close()
    out = []
    while True:
        line = p.stdout.readline()
        if not line:
            break
        # Only buffer 100 lines - used for errors
        if len(out) < 100:
            out.append(line)
        m = _TRANSFERRED_P.match(line)
        if m:
            yield _transferred_bytes_for_match(m), None
        elif "nothing to transfer" in line:
            yield -1, None
        elif verbose:
            yield None, line.rstrip()
    exit_code = p.wait()
    if exit_code != 0:
        raise _CopyError(exit_code, "".join(out))


_SIZE = {
    "B": 1,
    "KiB": 1024,
    "MiB": pow(1024, 2),
    "GiB": pow(1024, 3),
    "TiB": pow(1024, 4),
    "PiB": pow(1024, 5),
}


def _transferred_bytes_for_match(m: re.Match[str]):
    n = float(m.group(1))
    try:
        return _SIZE[m.group(2)] * n
    except KeyError:
        assert False, m.string


def _copy_from(args: Args):
    # TODO: Deferring substantial functionality of supporting remote run
    # listings and filters. Currently only supporting a non-filterable,
    # non-previewable copy of all remote runs.
    assert args.src
    if not args.all:
        cli.exit_with_error("'--all' is required when using '-s / --source'")
    if args.where:
        cli.exit_with_error("'--where' cannot be used with '-s / --source'")
    _verify_copy_from(args)
    with cli.Progress() as p:
        task = p.add_task("Copying runs")
        for copied, total in _rclone_copy_from(
            args.src, runs_dir(), excludes=["*.project", ".deleted"]
        ):
            p.update(task, completed=copied, total=total)
    # TODO: Deferring even reasonable UI like "copied N runs" as we're
    # just copying blindly from the src with the applied excludes
    # (above)
    cli.err(f"Copied runs")


def _verify_copy_from(args: Args):
    if args.yes:
        return
    cli.err(f"You are about copy all runs from {args.src}")
    cli.err()
    if not cli.confirm(f"Continue?"):
        raise SystemExit(0)


_TRANSFERRED2_P = re.compile(r"-Transferred:\s+([\d\.]+) ([\w]+) / ([\d\.]+) ([\w]+)")


def _rclone_copy_from(src: str, dest: str, excludes: list[str]):
    exclude_opts = flatten([["--exclude", pattern] for pattern in excludes])
    cmd = _rclone_cmd(
        [
            "copy",
            src,
            dest,
            "--progress",
            "--stats",
            "100ms",
        ]
        + exclude_opts
    )
    p = subprocess.Popen(
        cmd,
        text=True,
        stderr=subprocess.STDOUT,
        stdout=subprocess.PIPE,
    )
    assert p.stdout
    while True:
        line = p.stdout.readline()
        if not line:
            break
        m = _TRANSFERRED2_P.search(line)
        if m:
            yield (
                _parse_bytes(m.group(1), m.group(2), line),
                _parse_bytes(m.group(3), m.group(4), line),
            )
            time.sleep(0.1)
    result = p.wait()
    if result != 0:
        raise RuntimeError(result)


def _parse_bytes(s: str, unit: str, line: str):
    num = float(s)
    if unit == "KiB":
        return int(num * 1024)
    elif unit == "B":
        return int(num)
    else:
        assert False, (line, unit)
