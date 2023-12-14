# SPDX-License-Identifier: Apache-2.0

from typing import *

from .file_select import FileSelectRule

import logging
import re
import os
import subprocess

from . import file_util
from . import util

__all__ = [
    "CommitReadError",
    "FileStatus",
    "GitCheckResult",
    "GitNotInstalled",
    "NoCommit",
    "NoVCS",
    "Scheme",
    "UnsupportedRepo",
    "check_git_ls_files",
    "commit_for_dir",
    "git_project_select_rules",
    "git_version",
    "ls_files",
    "project_select_rules",
    "status",
]

# See https://github.com/guildai/guildai/issues/471 for details
GIT_LS_FILES_TARGET_VER = (2, 32, 0)


class UnsupportedRepo(Exception):
    pass


class NoVCS(Exception):
    pass


class GitNotInstalled(Exception):
    pass


_Arg = str | Callable[[], str]


class Scheme:
    def __init__(
        self,
        name: str,
        commit_cmd: list[_Arg],
        commit_pattern: Pattern[str],
        commit_ok_errors: list[int],
        status_cmd: list[_Arg],
        status_pattern: Pattern[str],
        status_ok_errors: list[int],
    ):
        self.name = name
        self.commit_cmd = commit_cmd
        self.commit_pattern = commit_pattern
        self.commit_ok_errors = commit_ok_errors
        self.status_cmd = status_cmd
        self.status_pattern = status_pattern
        self.status_ok_errors = status_ok_errors


def _git_exe():
    exe = util.which("git")
    if not exe:
        raise GitNotInstalled()
    return exe


COMMIT_INFO_SCHEMES = [
    Scheme(
        "git",
        commit_cmd=[_git_exe, "log", "-1", "."],
        commit_pattern=re.compile(r"commit ([a-f0-9]+)"),
        commit_ok_errors=[128],
        status_cmd=[_git_exe, "status", "-s"],
        status_pattern=re.compile(r"(.)"),
        status_ok_errors=[],
    )
]


class FileStatus(NamedTuple):
    """Represents a file in the result of `status()`.

    `status` is a two character status code. This follows the git
    convention as documented in `git status --help` with the exception
    that an empty space char (' ') in the git spec becomes an underscore
    char ('_') in this spec.

      _ = unmodified
      M = modified
      A = added
      D = deleted
      R = renamed
      C = copied
      U = updated but unmerged

    X           Y    Meaning
    -------------------------------------------------
    _        [AMD]   not updated
    M        [ MD]   updated in index
    A        [ MD]   added to index
    D                deleted from index
    R        [ MD]   renamed in index
    C        [ MD]   copied in index
    [MARC]      _    index and work tree matches
    [ MARC]     M    work tree changed since index
    [ MARC]     D    deleted in work tree
    [ D]        R    renamed in work tree
    [ D]        C    copied in work tree
    -------------------------------------------------
    D           D    unmerged, both deleted
    A           U    unmerged, added by us
    U           D    unmerged, deleted by them
    U           A    unmerged, added by them
    D           U    unmerged, deleted by us
    A           A    unmerged, both added
    U           U    unmerged, both modified
    -------------------------------------------------
    ?           ?    untracked
    !           !    ignored

    """

    status: str
    path: str
    orig_path: str | None


log = logging.getLogger("guild")


class NoCommit(Exception):
    pass


class CommitReadError(Exception):
    pass


def commit_for_dir(dir: str):
    """Returns a tuple of commit and workspace status.

    Raises NoCommit if a commit is not available.
    """
    dir = os.path.abspath(dir)
    for scheme in COMMIT_INFO_SCHEMES:
        commit = _apply_scheme(
            dir,
            scheme.commit_cmd,
            scheme.commit_pattern,
            scheme.commit_ok_errors,
        )
        if commit is None:
            raise NoCommit(dir)
        status = _apply_scheme(
            dir,
            scheme.status_cmd,
            scheme.status_pattern,
            scheme.status_ok_errors,
        )
        return _format_commit(commit, scheme), _format_status(status)
    raise NoCommit(dir)


def _resolve_arg(val: _Arg):
    if callable(val):
        return str(val())
    return val


def _apply_scheme(
    repo_dir: str,
    cmd_template: list[_Arg],
    pattern: Pattern[str],
    ok_errors: list[int],
):
    """Returns status str for scheme if status cmd succeeds.

    If the status pattern does match command output returns None.

    Zero exit code or any exit code in `ok_errors` is considered a successful
    application. Otherwise raises `CommitReadError`.
    """
    cmd = [_resolve_arg(arg).format(repo=repo_dir) for arg in cmd_template]
    log.debug("vcs scheme cmd for repo %s: %s", repo_dir, cmd)
    try:
        out = subprocess.check_output(
            cmd,
            cwd=repo_dir,
            env=os.environ,
            stderr=subprocess.STDOUT,
        )
    except OSError as e:
        if e.errno == 2:
            return None
        raise CommitReadError(e) from e
    except subprocess.CalledProcessError as e:
        if e.returncode in ok_errors:
            return None
        raise CommitReadError(e, e.output) from e
    else:
        out = out.decode("ascii", errors="replace")
        log.debug("vcs scheme result: %s", out)
        m = pattern.match(out)
        if not m:
            return None
        return m.group(1)


def _format_commit(commit: str, scheme: Scheme):
    return f"{scheme.name}:{commit}"


def _format_status(status: Optional[str]):
    return bool(status)


def ls_files(dir: str):
    try:
        return util.try_apply([_try_git_source_iter], dir)
    except util.TryFailed:
        raise UnsupportedRepo(dir) from None


def _try_git_source_iter(dir: str):
    """Class that conforms to the util.try_apply scheme for iterating source.

    Raises `util.TryFailed` if git does not provide a list of files.
    """
    tracked = _try_git_ls_files(dir, untracked=False)
    untracked = _try_git_ls_files(dir, untracked=True)
    return tracked + untracked


def _try_git_ls_files(dir: str, untracked: bool = False):
    cmd = [_git_exe.read(), "ls-files"]
    if untracked:
        cmd.extend(["--other", "--exclude-standard"])
    try:
        out = subprocess.check_output(cmd, cwd=dir, stderr=subprocess.STDOUT)
    except subprocess.CalledProcessError as e:
        if log.getEffectiveLevel() <= logging.DEBUG:
            log.error("error listing git files (%i)", e.returncode)
            log.error(e.stdout)
        raise util.TryFailed() from None
    else:
        return _parse_git_ls_files(out)


def _parse_git_ls_files(out: bytes):
    return util.split_lines(out.decode("utf-8", errors="ignore"))


def status(dir: str, ignored: bool = False):
    try:
        return util.try_apply([_try_git_status], dir, ignored)
    except util.TryFailed:
        raise UnsupportedRepo(dir) from None


def _try_git_status(dir: str, ignored: bool):
    ignored_args = ["--ignored=matching"] if ignored else []
    cmd = [_git_exe.read(), "status", "--short", "--renames"] + ignored_args + ["."]
    try:
        out = subprocess.check_output(cmd, cwd=dir, stderr=subprocess.STDOUT)
    except subprocess.CalledProcessError as e:
        if log.getEffectiveLevel() <= logging.DEBUG:
            log.error("error listing git files (%i)", e.returncode)
            log.error(e.stdout)
        raise util.TryFailed() from None
    else:
        return _parse_git_status(out)


def _parse_git_status(out: bytes):
    lines = util.split_lines(out.decode("utf-8", errors="ignore"))
    return [_decode_git_status_line(line) for line in lines]


def _decode_git_status_line(status_line: str):
    status = _status_code_for_git_status_line(status_line)
    rest = status_line[3:]
    path, orig_path = _split_git_file_status_path(rest)
    return FileStatus(status, path, orig_path)


def _status_code_for_git_status_line(status_line: str):
    """Returns the XY status git status.

    Git status char ' ' (empty space) is replaced with an underscore
    per the MergeFile status spec above.

    See `git status --help` for details.

    """
    assert len(status_line) >= 2, status_line
    return status_line[:2].replace(" ", "_")


def _split_git_file_status_path(path: str):
    parts = path.split(" -> ", 1)
    if len(parts) == 2:
        return parts[1], parts[0]
    return parts[0], None


def project_select_rules(project_dir: str):
    # Only supporting Git based rules
    return git_project_select_rules(project_dir)


def git_project_select_rules(project_dir: str) -> list[FileSelectRule]:
    git_ignored = _git_ls_ignored(project_dir, extended_patterns_file=".guildignore")
    ignored_dirs = _dirs_for_git_ignored(git_ignored, project_dir)
    return [
        # Ignore directories first as an optimization
        FileSelectRule(False, [".git"] + ignored_dirs, "dir"),
        # Git ignore select selects everything that isn't ignored -
        # this must be placed before rules that exclude patterns
        _GitignoreSelectRule(git_ignored),
        FileSelectRule(False, [".git*", ".guildignore"]),
    ]


def _git_ls_ignored(cwd: str, extended_patterns_file: str):
    try:
        version = git_version()
    except GitNotInstalled:
        _maybe_warn_git_not_installed(cwd)
        raise NoVCS(cwd) from None
    else:
        if version < GIT_LS_FILES_TARGET_VER:
            return _git_ls_ignored_legacy(cwd, extended_patterns_file)
        return _git_ls_ignored_(cwd, extended_patterns_file)


def _maybe_warn_git_not_installed(cwd: str):
    """Prints a warning if `cwd` is part of a Git repo.

    Warning can be disabled by setting env `NO_WARN_GIT_MISSING` to `1`.
    """
    if os.getenv("NO_WARN_GIT_MISSING") == "1":
        return
    if _is_git_repo(cwd):
        log.warning(
            "The current project appears to use Git for version control\n"
            "but git is not available on the system path. To apply Git's source\n"
            "code rules to this run, install Git [1] or specify the Git executable\n"
            "in Guild user config [2].\n"
            "\n"
            "[1] https://git-scm.com/book/en/v2/Getting-Started-Installing-Git)\n"
            "[2] https://my.guild.ai/t/user-config-reference\n"
            "\n"
            "To disable this warning, set 'NO_WARN_GIT_MISSING=1'\n"
        )


def _is_git_repo(dir: str):
    """Returns True if `dir` is managed under a Git repo."""
    return file_util.find_up(os.path.join(".git", "HEAD"), dir) is not None


def _git_ls_ignored_legacy(cwd: str, extended_patterns_file: str):
    ignored_files = _git_ls_ignored_(cwd, extended_patterns_file, directory_flag=False)
    ignored_dirs = _git_ls_ignored_(cwd, extended_patterns_file, directory_flag=True)
    return ignored_files + ignored_dirs


def _git_ls_ignored_(
    cwd: str, extended_patterns_file: str, directory_flag: bool = True
):
    cmd = _git_ls_ignored_cmd(extended_patterns_file, directory_flag)
    log.debug("cmd for ls ignored in %s: %s", cwd, cmd)
    try:
        out = subprocess.check_output(cmd, cwd=cwd, stderr=subprocess.STDOUT)
    except subprocess.CalledProcessError as e:
        if e.returncode not in (128,):
            # 128: not a git repo -> ignore
            log.warning(
                "error listing ignored files (%i): %s",
                e.returncode,
                e.output.decode(),
            )
            if log.getEffectiveLevel() <= logging.DEBUG:
                log.error(e.stdout)
        raise NoVCS(cwd, (e.returncode, e.stdout)) from None
    else:
        return _parse_git_ls_files(out)


def _git_ls_ignored_cmd(extended_patterns_file: str, directory_flag: bool):
    """Returns the Git command for listing ignored files.

    Contains `ls-files` with options for listing ignored files. Uses
    `--exclude-standard` to apply the standard gitignore rules to the
    result.

    If `extended_patterns_file` is specified, includes `-x` args for
    each line in file.

    If `directory_flag` is True, includes `--directory` option.
    """
    cmd: list[str] = [_git_exe.read(), "ls-files", "-ioc", "--exclude-standard"]
    if directory_flag:
        cmd.append("--directory")
    if extended_patterns_file:
        cmd.extend(_exclude_args_for_patterns_file(extended_patterns_file))
    return cmd


def _exclude_args_for_patterns_file(patterns_file: str):
    return [
        arg
        for pattern in _exclude_patterns_file_entries(patterns_file)
        for arg in ["-x", pattern]
    ]


def _exclude_patterns_file_entries(src: str) -> list[str]:
    try:
        f = open(src)
    except FileNotFoundError:
        return []
    else:
        with f:
            lines = [line.strip() for line in f]
        return [line for line in lines if line and not line.startswith("#")]


def _dirs_for_git_ignored(ignored: list[str], root_dir: str):
    return [
        _strip_trailing_slash(path)
        for path in ignored
        if os.path.isdir(os.path.join(root_dir, path))
    ]


def _strip_trailing_slash(path: str):
    return path[:-1] if path[-1:] in ("/", "\\") else path


class _GitignoreSelectRule(FileSelectRule):
    """Higher order selection rule using git ignored files.

    This is a 'select everything except ignored' rule and can be used
    in place of a select '*' select rule - with the exception that git
    ignored files are not selected.
    """

    def __init__(self, ignored: list[str]):
        super().__init__(True, [])
        self.ignored = set(_normalize_paths(ignored))

    def __str__(self):
        return "gitignore + guildignore patterns"

    def test(self, src_root: str, relpath: str):
        # This is a 'select everything except ignored' rule so we
        # return `True` to select anything that isn't in our list of
        # ignored. This could alternatively be a `False` for anything
        # in ignored, but this would require an explicit select '*'
        # rule to precede it.
        if relpath not in self.ignored:
            return True, None
        return None, None


def _normalize_paths(paths: list[str]):
    return [os.path.normpath(path) for path in paths]


class GitCheckResult:
    def __init__(
        self,
        git_version: tuple[int, int, int],
        git_exe: str,
        out: str,
        error: Optional[str] = None,
    ):
        self.git_version = git_version
        self.git_exe = git_exe
        self.error = error
        self.out = out

    @property
    def formatted_git_version(self):
        v = self.git_version
        return f"{v[0]}.{v[1]}.{v[2]}"


def check_git_ls_files():
    """Checks Git for a specific ls-files behavior.

    As of 2.32 the behavior of the command:

        git ls-files -ioc --exclude-standard --directory

    includes directories that should be ignored when all of their
    contained files are ignored. This appears to be an optimization in
    the generated output for this command. Guild takes advantage of this
    by excluding these directories from its source code copy rules so as
    not to traverse their files.

    Older versions of Git, however, behave differently -- they omit such
    directories *and the ignored files in them* from the command output.
    If relied on, this behavior results in Guild missing ignored files
    in its inferred source code select rules.

    This function is used to explicitly test the behavior Git on the
    system. It relies on the system PATH to locate the `git` executable
    by default. User can provide an explicit Git executable in user
    config under `git.executable`.

    Returns an instance of `GitCheckResult`. If the expected behavior of
    Git is incorrect, the result error is specified in the `error`
    attribute.
    """
    git_exe = _git_exe()
    project_dir = _init_git_ls_files_sample_project()
    out = subprocess.check_output(
        [
            git_exe,
            "ls-files",
            "-ioc",
            "--exclude-standard",
            "--directory",
        ],
        cwd=project_dir,
    )
    error = _git_check_error_for_out(out)
    return GitCheckResult(git_version(), git_exe, out.decode(), error)


def _git_check_error_for_out(out: bytes):
    if out != b"files/\nfiles/foo.txt\n":
        return f"unexpected output for ls-files: {out.decode().rstrip()}"
    return None


def _init_git_ls_files_sample_project():
    """Initializes a sample project for git ls-files check.

    Project is an initialized Git repo consisting of:

      - subdirectory `files` containing a single file `foo.txt`
      - `.gitignore` containing a single line `*.txt`

    Returns the project directory.
    """
    project_dir = file_util.make_temp_dir("guild-check-")
    _ = subprocess.check_output([_git_exe(), "init"], cwd=project_dir)
    os.mkdir(os.path.join(project_dir, "files"))
    file_util.touch(os.path.join(project_dir, "files", "foo.txt"))
    with open(os.path.join(project_dir, ".gitignore"), "w") as f:
        f.write("*.txt\n")
    return project_dir


def git_version():
    cmd = [_git_exe(), "--version"]
    try:
        out = subprocess.check_output(cmd).decode().strip()
    except (FileNotFoundError, subprocess.CalledProcessError) as e:
        if log.getEffectiveLevel() <= logging.DEBUG:
            log.exception("error getting git version")
        raise GitNotInstalled() from e
    else:
        m = re.search(r"([0-9]+)\.([0-9]+)\.([0-9]+)", out)
        assert m and m.lastindex == 3, out
        return cast(tuple[int, int, int], tuple(int(x) for x in m.groups()))
