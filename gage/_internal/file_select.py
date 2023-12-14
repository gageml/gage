# SPDX-License-Identifier: Apache-2.0

from typing import *

import fnmatch
import errno
import glob
import logging
import os
import re
import shutil

from .shlex_util import shlex_quote
from .shlex_util import shlex_split

from .file_util import ensure_dir
from .file_util import is_text_file
from .file_util import standardize_path


__all__ = [
    "FileCopyHandler",
    "FileSelectTestFunc",
    "FileSelectTest",
    "FileSelectResult",
    "FileSelectRuleType",
    "FileSelectRule",
    "FileSelectResults",
    "FileSelect",
    "DisabledFileSelect",
    "copy_files",
    "copy_tree",
    "exclude",
    "include",
    "parse_patterns",
    "select_files",
]

log = logging.getLogger(__name__)

FileSelectTestFunc = Callable[..., bool | None]


class FileSelectTest:
    def __init__(self, name: str, test_f: FileSelectTestFunc, *test_args: Any):
        self.name = name
        self.test_f = test_f
        self.test_args = test_args

    def __call__(self):
        return self.test_f(*self.test_args)


FileSelectResult = tuple[bool | None, FileSelectTest | None]

FileSelectRuleType = Literal["text", "binary", "dir"]


class FileSelectRule:
    def __init__(
        self,
        result: bool,
        patterns: list[str] | str,
        type: FileSelectRuleType | None = None,
        regex: bool = False,  # treat patterns as regex, otherwise globs
        sentinel: str = "",
        size_gt: int | None = None,
        size_lt: int | None = None,
        max_matches: int | None = None,
    ):
        self.result = result
        self.patterns = _init_patterns(patterns, regex)
        self.regex = regex
        self._patterns_match = _patterns_match_f(self.patterns, regex)
        self.type = _validate_type(type)
        self.sentinel = sentinel
        self.size_gt = size_gt
        self.size_lt = size_lt
        self.max_matches = max_matches
        self._matches = 0

    def __str__(self):
        parts = [self.result and "include" or "exclude"]
        if self.type:
            parts.append(self.type)
        parts.append(", ".join([_quote_pattern(p) for p in self.patterns]))
        extras = self._format_file_select_rule_extras()
        if extras:
            parts.append(extras)
        return " ".join(parts)

    def _format_file_select_rule_extras(self):
        parts = []
        if self.regex:
            parts.append("regex")
        if self.sentinel:
            parts.append(f"containing {_quote_pattern(self.sentinel)}")
        if self.size_gt:
            parts.append(f"size > {self.size_gt}")
        if self.size_lt:
            parts.append(f"size < {self.size_lt}")
        if self.max_matches:
            parts.append(f"max match {self.max_matches}")
        return ", ".join(parts)

    @property
    def matches(self):
        return self._matches

    def test(self, src_dir: str, relpath: str) -> FileSelectResult:
        """Returns a tuple of result and applicable test.

        Applicable test can be used as a reason for the result -
        e.g. to provide details to a user on why a particular file was
        selected or not.
        """
        fullpath = os.path.join(src_dir, relpath)
        tests = [
            FileSelectTest("max matches", self._test_max_matches),
            FileSelectTest("pattern", self._test_patterns, relpath),
            FileSelectTest("type", self._test_type, fullpath),
            FileSelectTest("size", self._test_size, fullpath),
        ]
        for test in tests:
            if not test():
                return None, test
        self._matches += 1
        return self.result, None

    def _test_max_matches(self):
        if self.max_matches is None:
            return True
        return self._matches < self.max_matches

    def _test_patterns(self, path: str):
        return self._patterns_match(path)

    def _test_type(self, path: str):
        if self.type is None:
            return True
        if self.type == "text":
            return _is_text_file(path)
        if self.type == "binary":
            return not _is_text_file(path)
        if self.type == "dir":
            return self._test_dir(path)
        assert False, self.type

    def _test_dir(self, path: str):
        if not os.path.isdir(path):
            return False
        if self.sentinel:
            return len(glob.glob(os.path.join(path, self.sentinel))) > 0
        return True

    def _test_size(self, path: str):
        if self.size_gt is None and self.size_lt is None:
            return True
        size = _file_size(path)
        if size is None:
            return True
        if self.size_gt and size > self.size_gt:
            return True
        if self.size_lt and size < self.size_lt:
            return True
        return False


def _init_patterns(patterns: list[str] | str, regex: bool):
    if isinstance(patterns, str):
        patterns = [patterns]
    return patterns if regex else _native_paths(patterns)


def _native_paths(patterns: list[str]):
    return [p.replace("/", os.path.sep) for p in patterns]


def _patterns_match_f(patterns: list[str], regex: bool):
    return regex and _regex_match_f(patterns) or _fnmatch_f(patterns)


def _regex_match_f(patterns: list[str]):
    compiled = [re.compile(p) for p in patterns]

    def f(path: str):
        return any((p.match(standardize_path(path)) for p in compiled))

    return f


def _fnmatch_f(patterns: list[str]):
    def f(path: str):
        return any((_fnmatch(path, p) for p in patterns))

    return f


def _fnmatch(path: str, pattern: str):
    if os.path.sep not in pattern:
        path = os.path.basename(path)
    pattern = _strip_leading_path_sep(pattern)
    return fnmatch.fnmatch(path, pattern)


def _strip_leading_path_sep(pattern: str):
    while pattern:
        if pattern[0] != os.path.sep:
            break
        pattern = pattern[1:]
    return pattern


def _validate_type(type: str | None):
    valid = ("text", "binary", "dir")
    if type is not None and type not in valid:
        raise ValueError(
            f"invalid value for type {type!r}: expected one of {', '.join(valid)}"
        )
    return type


def _quote_pattern(s: str):
    return shlex_quote(s) if " " in s else s


def _is_text_file(path: str):
    try:
        return is_text_file(path)
    except OSError as e:
        log.warning("could not check for text file %s: %s", path, e)
        return False


def _file_size(path: str):
    try:
        return os.path.getsize(path)
    except OSError:
        return None


FileSelectResults = list[tuple[FileSelectResult, FileSelectRule]]


class FileSelect:
    _disabled = None

    def __init__(self, rules: list[FileSelectRule]):
        self.rules = rules

    @property
    def disabled(self):
        if self._disabled is None:
            self._disabled = self._init_disabled()
        return self._disabled

    def _init_disabled(self):
        """Returns True if file select is disabled.

        This is an optimization to disable file select by appending an
        exclude '*' to a rule set.

        Assumes not disabled until finds a disable all pattern (untyped
        match of '*'). Disable pattern can be reset by any subsequent
        include pattern.
        """
        disabled = False
        for rule in self.rules:
            if rule.result:
                disabled = False
            elif "*" in rule.patterns and rule.type is None:
                disabled = True
        return disabled

    def select_file(
        self,
        src_dir: str,
        relpath: str,
    ) -> tuple[bool, FileSelectResults]:
        """Apply rules to file located under a src dir with relpath.

        All rules are applied to the file. The last rule to apply
        (i.e. its `test` method returns a non-None value) determines
        whether or not the file is selected - selected if test returns
        True, not selected if returns False.

        If no rules return a non-None value, the file is not selected.

        Returns a tuple of the selected flag (True or False) and list
        of applied rules and their results (two-tuples).
        """
        test_results = [
            (rule.test(src_dir, relpath), rule)
            for rule in self.rules
            if rule.type != "dir"
        ]
        result, _test = _reduce_file_select_results(test_results)
        return result is True, test_results

    def prune_dirs(
        self, root: str, relroot: str, dirs: list[str]
    ) -> list[tuple[str, FileSelectResults]]:
        """Deletes from dirs values that excluded as 'dir' type.

        Returns a list of pruned dir / file select results. The file
        select results are the cause of the excluded dir.
        """
        pruned: list[tuple[str, FileSelectResults]] = []
        for name in sorted(dirs):
            last_select_result: FileSelectResult | None = None
            last_select_result_rule: FileSelectRule | None = None
            relpath = os.path.join(relroot, name)
            for rule in self.rules:
                if rule.type != "dir":
                    continue
                selected, _ = select_result = rule.test(root, relpath)
                if selected is not None:
                    last_select_result = select_result
                    last_select_result_rule = rule
            if last_select_result and last_select_result[0] is False:
                assert last_select_result_rule
                log.debug("skipping directory %s", relpath)
                select_results: FileSelectResults = [
                    (last_select_result, last_select_result_rule)
                ]
                pruned.append((name, select_results))
                dirs.remove(name)
        return pruned


def _reduce_file_select_results(results: FileSelectResults) -> FileSelectResult:
    for (result, test), _rule in reversed(results):
        if result is not None:
            return result, test
    return None, None


class DisabledFileSelect(FileSelect):
    def __init__(self):
        super().__init__([])

    @property
    def disabled(self):
        return True


def include(patterns: list[str], **kw: Any):
    return FileSelectRule(True, patterns, **kw)


def exclude(patterns: list[str], **kw: Any):
    return FileSelectRule(False, patterns, **kw)


class FileCopyHandler:
    def copy(
        self,
        src_dir: str,
        dest_dir: str,
        select_results: FileSelectResults | None = None,
    ):
        if select_results:
            log.debug("%s selected for copy: %s", src_dir, select_results)
        log.debug("copying %s to %s", src_dir, dest_dir)
        ensure_dir(os.path.dirname(dest_dir))
        self._try_copy_file(src_dir, dest_dir)

    def _try_copy_file(self, src_filename: str, dest_filename: str):
        try:
            shutil.copyfile(src_filename, dest_filename)
            shutil.copymode(src_filename, dest_filename)
        except IOError as e:
            if e.errno != errno.EEXIST:
                if not self.handle_copy_error(e, src_filename, dest_filename):
                    raise

    def ignore(self, filename: str, results: FileSelectResults | None):
        """Called when a file is ignored for copy.

        `results` is the file select results that caused the decision to
        ignore the file. If `result` is None it can be inferred that no
        select rules were applied and the copy behavior is to ignore all
        files by default.
        """
        pass

    def handle_copy_error(
        self,
        error: Exception,
        src_filename: str,
        dest_filename: str,
    ):
        return False

    def close(self):
        pass


def copy_files(
    src_dir: str,
    dest_dir: str,
    files: list[str],
    select: FileSelect | None = None,
    handler: FileCopyHandler | None = None,
):
    """Copies a list of files located under a source directory.

    `files` is a list of relative paths under `src_dir`. Files are
    copied to `dest` as their relative paths.

    `handler_cls` is an optional class to create a `FileCopyHandler`
    instance. This is used to filter and otherwise handle a file copy.
    The default is a "select all" handler that copies each file in
    `files` without additional behavior.
    """
    if select and select.disabled:
        return
    handler = handler or FileCopyHandler()
    try:
        _copyfiles_impl(src_dir, dest_dir, files, select, handler)
    finally:
        handler.close()


def _copyfiles_impl(
    src_dir: str,
    dest_dir: str,
    files: list[str],
    select: FileSelect | None,
    handler: FileCopyHandler,
):
    for path in files:
        file_src = os.path.join(src_dir, path)
        file_dest = os.path.join(dest_dir, path)
        if select is None:
            handler.copy(file_src, file_dest)
        else:
            selected, results = select.select_file(src_dir, path)
            if selected:
                handler.copy(file_src, file_dest, results)
            else:
                handler.ignore(file_src, results)


def copy_tree(
    src_dir: str,
    dest_dir: str,
    select: FileSelect | None = None,
    handler: FileCopyHandler | None = None,
    follow_links: bool = True,
):
    """Copies files from src dir to dest dir for a FileSelect.

    If follow_links is True (default), follows linked directories when
    copying the tree.

    A file select spec may be specified to control the copy process. The
    select determines if a directory is scanned and whether or not
    scanned files are copied.

    A handler may be specified to implement the copy logic, including
    logging and handling ignored files.
    """
    if select and select.disabled:
        return
    handler = handler or FileCopyHandler()
    try:
        _copytree_impl(src_dir, dest_dir, select, handler, follow_links)
    finally:
        handler.close()


def _copytree_impl(
    src_dir: str,
    dest_dir: str,
    select: FileSelect | None,
    handler: FileCopyHandler,
    follow_links: bool,
):
    for root, dirs, files in os.walk(src_dir, followlinks=follow_links):
        dirs.sort()
        relroot = _relpath(root, src_dir)
        pruned = _prune_dirs(src_dir, relroot, select, dirs)
        for name, select_results in pruned:
            handler.ignore(os.path.join(root, name), select_results)
        for name in sorted(files):
            selected, file_src, file_dest, select_results = _select_file_for_copy(
                src_dir, relroot, name, dest_dir, select
            )
            if selected:
                assert file_dest
                handler.copy(file_src, file_dest, select_results)
            else:
                handler.ignore(file_src, select_results)


def _relpath(path: str, start: str):
    if path == start:
        return ""
    return os.path.relpath(path, start)


def _prune_dirs(
    src_dir: str,
    relroot: str,
    select: FileSelect | None,
    dirs: list[str],
) -> list[tuple[str, FileSelectResults]]:
    if not select:
        return []
    return select.prune_dirs(src_dir, relroot, dirs) if select else []


def _select_file_for_copy(
    src_dir: str,
    relroot: str,
    name: str,
    dest_root: str,
    select: FileSelect | None,
) -> tuple[bool, str, str | None, FileSelectResults | None]:
    relpath = os.path.join(relroot, name)
    file_src = os.path.join(src_dir, relroot, name)
    if not select:
        return True, file_src, os.path.join(dest_root, relroot, name), None
    selected, results = select.select_file(src_dir, relpath)
    if selected:
        return True, file_src, os.path.join(dest_root, relroot, name), results
    return False, file_src, None, results


def parse_patterns(patterns: list[str]):
    rules = [_parse_pattern(*_split_pattern(p)) for p in patterns]
    return FileSelect(rules)


_PATTERN_EXCLUDE_P = re.compile(r"- *")


def _split_pattern(pattern: str):
    if pattern.startswith("\\-"):
        return pattern[1:], True
    m = _PATTERN_EXCLUDE_P.match(pattern)
    return (pattern[m.end() :], False) if m else (pattern, True)


def _parse_pattern(spec: str, result: bool):
    pattern, *tokens = shlex_split(spec)
    type = _file_type_for_tokens(tokens)
    size_lt, size_gt = _file_size_for_tokens(tokens)
    sentinel = _sentinel_for_tokens(tokens)
    max_matches = _max_matches_for_tokens(tokens)
    return FileSelectRule(
        result,
        _glob_to_re_pattern(pattern),
        regex=True,
        type=type,
        size_lt=size_lt,
        size_gt=size_gt,
        sentinel=sentinel,
        max_matches=max_matches,
    )


def _file_type_for_tokens(tokens: list[str]) -> FileSelectRuleType | None:
    for type in ("dir", "text", "binary"):
        if type in tokens:
            return type
    return None


_FILE_SIZE_TOKEN_P = re.compile(r"size([<>])(\d+)$")


def _file_size_for_tokens(tokens: list[str]) -> tuple[int | None, int | None]:
    size_lt = None
    size_gt = None
    for t in tokens:
        m = _FILE_SIZE_TOKEN_P.match(t)
        if m:
            if m.group(1) == ">":
                size_gt = int(m.group(2))
            else:
                assert m.group(1) == "<"
                size_lt = int(m.group(2))
    return size_lt, size_gt


def _sentinel_for_tokens(tokens: list[str]) -> str:
    for t in tokens:
        if t.startswith("sentinel="):
            return t[9:]
    return ""


_MAX_MATCHES_TOKEN_P = re.compile(r"max-matches=(\d+)$")


def _max_matches_for_tokens(tokens: list[str]) -> int | None:
    for t in tokens:
        m = _MAX_MATCHES_TOKEN_P.match(t)
        if m:
            return int(m.group(1))
    return None


_GLOB_MATCHER_P = re.compile(r"\*\*/?|\*|\?")


def _glob_to_re_pattern(pattern: str):
    re_parts = []
    path_sep = re.escape(os.path.sep)
    pos = 0
    for m in _GLOB_MATCHER_P.finditer(pattern):
        start, end = m.span()
        re_parts.append(re.escape(pattern[pos:start]))
        matcher = pattern[start:end]
        if matcher == "*":
            re_parts.append(rf"[^{path_sep}]*")
        elif matcher in ("**/", "**"):
            re_parts.append(r"(?:.+/)*")
        elif matcher == "?":
            re_parts.append(rf"[^{path_sep}]?")
        else:
            assert False, (matcher, pattern)
        pos = end
    re_parts.append(re.escape(pattern[pos:]))
    re_parts.append("$")
    return "".join(re_parts)


class _PreviewHandler(FileCopyHandler):
    def __init__(self, src_dir: str):
        self.src_dir = src_dir
        self.to_copy: list[tuple[str, FileSelectResults | None]] = []

    def copy(
        self,
        src_filename: str,
        dest_filename: str,
        select_results: FileSelectResults | None = None,
    ):
        src_relpath = os.path.relpath(src_filename, self.src_dir)
        self.to_copy.append((src_relpath, select_results))

    def ignore(self, filename: str, results: FileSelectResults | None):
        pass

    def handle_copy_error(
        self,
        error: Exception,
        src_filename: str,
        dest_filename: str,
    ):
        return False

    def close(self):
        pass


def select_files(
    src_dir: str,
    select: FileSelect | None = None,
    follow_links: bool = True,
):
    handler = _PreviewHandler(src_dir)
    copy_tree(src_dir, "", select, handler, follow_links)
    return sorted([path for path, _result in handler.to_copy])
