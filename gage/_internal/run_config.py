# SPDX-License-Identifier: Apache-2.0

from typing import *

import difflib
import logging
import os
import re

from .types import *

from . import file_select

__all__ = [
    "RunConfig",
    "RunConfigValue",
    "UnsupportedFileFormat",
    "apply_config",
    "match_keys",
    "read_file_config",
    "read_project_config",
]

log = logging.getLogger(__name__)


class UnsupportedFileFormat(Exception):
    pass


def read_project_config(src_dir: str, opdef: OpDef):
    config: dict[str, RunConfigValue] = {}
    for opdef_config in opdef.get_config():
        parsed_paths = _parse_paths(opdef_config.get_keys())
        files_config = _selected_files_config(src_dir, parsed_paths)
        keys = _select_keys(src_dir, files_config, parsed_paths)
        for path, file_config in files_config:
            config.update(file_config)
    return config


def apply_config(config: RunConfig, opdef: OpDef, dest_dir: str):
    applied: list[tuple[str, UnifiedDiff]] = []
    for opdef_config in opdef.get_config():
        parsed_paths = _parse_paths(opdef_config.get_keys())
        files_config = _selected_files_config(dest_dir, parsed_paths)
        keys = _select_keys(dest_dir, files_config, parsed_paths)
        for path, file_config in files_config:
            diff = _apply_file_config(config, keys, file_config, path, dest_dir)
            if diff:
                applied.append((path, diff))
    return applied


class ParsedPath(NamedTuple):
    file_pattern: str | None
    file_exclude: bool
    key_pattern: str | None
    key_exclude: bool


def _parse_paths(paths: list[str]) -> list[ParsedPath]:
    return [_parse_path(path) for path in paths]


def _parse_path(path: str) -> ParsedPath:
    if not path:
        raise ValueError("path cannot be empty")
    path, excluded = _strip_excluded(path)
    file_part, key_part = _split_path(path)
    return ParsedPath(
        file_part,
        False if key_part else excluded,
        key_part if key_part is not None else "*" if not excluded else None,
        excluded,
    )


_EXCLUDE_P = re.compile(r"- *")


def _strip_excluded(pattern: str):
    if pattern.startswith("\\-"):
        return pattern[1:], False
    m = _EXCLUDE_P.match(pattern)
    return (pattern[m.end() :], True) if m else (pattern, False)


def _split_path(path: str) -> tuple[str | None, str | None]:
    file_pattern, *rest = path.split("#", 1)
    if not file_pattern:
        return None, rest[0]
    if not rest:
        return file_pattern, None
    return file_pattern or None, rest[0]


def _select_files(src_dir: str, parsed_paths: list[ParsedPath]):
    select = file_select.parse_patterns(
        [
            _select_pattern(p.file_pattern, p.file_exclude)
            for p in parsed_paths
            if p.file_pattern
        ]
    )
    return file_select.select_files(src_dir, select)


def _select_pattern(path: str, exclude: bool):
    return "-" + path if exclude else path


def read_file_config(filename: str) -> RunConfig:
    _, ext = os.path.splitext(filename)
    if ext == ".py":
        from .run_config_py import PythonConfig

        return PythonConfig(open(filename).read())
    raise UnsupportedFileFormat(filename)


def match_keys(include: list[str], exclude: list[str], keys: list[str] | RunConfig):
    include_p = [_compile_match_pattern(p) for p in include]
    exclude_p = [_compile_match_pattern(p) for p in exclude]
    return [key for key in keys if _match_key(include_p, exclude_p, key)]


_MATCHER_P = re.compile(r"\*\*\.?|\*|\?")


def _compile_match_pattern(pattern: str):
    re_parts = []
    path_sep = re.escape(".")
    pos = 0
    for m in _MATCHER_P.finditer(pattern):
        start, end = m.span()
        re_parts.append(re.escape(pattern[pos:start]))
        matcher = pattern[start:end]
        if matcher == "*":
            re_parts.append(rf"[^{path_sep}]*")
        elif matcher in ("**.", "**"):
            re_parts.append(r"(?:.+\.)*")
        elif matcher == "?":
            re_parts.append(rf"[^{path_sep}]?")
        else:
            assert False, (matcher, pattern)
        pos = end
    re_parts.append(re.escape(pattern[pos:]))
    re_parts.append("$")
    return re.compile("".join(re_parts))


def _match_key(include: list[Pattern[str]], exclude: list[Pattern[str]], key: str):
    return any(p.match(key) for p in include) and not any(p.match(key) for p in exclude)


def _selected_files_config(dest_dir: str, parsed_paths: list[ParsedPath]):
    files_config: list[tuple[str, RunConfig]] = []
    for path in _select_files(dest_dir, parsed_paths):
        filename = os.path.join(dest_dir, path)
        try:
            config = read_file_config(filename)
        except Exception as e:
            log.warning("Cannot load configuration for \"%s\": %s", filename)
            print(f"WARNING: {e}")
        else:
            files_config.append((path, config))
    return files_config


def _select_keys(
    src_dir: str,
    files_config: list[tuple[str, RunConfig]],
    parsed_paths: list[ParsedPath],
):
    file_select = _file_select_for_keys(src_dir, parsed_paths)
    key_select = _key_select(parsed_paths)
    keys: Set[str] = set()
    for file_path, file_config in files_config:
        if file_select(file_path):
            keys.update([key for key in file_config if key_select(key)])
    return list(keys)


def _file_select_for_keys(src_dir: str, paths: list[ParsedPath]):
    select = file_select.parse_patterns(
        [
            _select_pattern(p.file_pattern, p.file_exclude)
            for p in paths
            if p.file_pattern
        ]
    )

    def f(path: str):
        return select.select_file(src_dir, path)[0]

    return f


def _key_select(paths: list[ParsedPath]):
    include_p = [
        _compile_match_pattern(p.key_pattern)
        for p in paths
        if p.key_pattern and not p.key_exclude
    ]
    exclude_p = [
        _compile_match_pattern(p.key_pattern)
        for p in paths
        if p.key_pattern and p.key_exclude
    ]

    def f(key: str):
        return _match_key(include_p, exclude_p, key)

    return f


def _apply_file_config(
    config: RunConfig,
    keys: list[str],
    file_config: RunConfig,
    path: str,
    dest_dir: str,
):
    _apply_config_vals(config, keys, file_config)
    filename = os.path.join(dest_dir, path)
    v1_lines = _read_file_lines(filename)
    v2_lines = _applied_config_lines(file_config)
    diff = _diff_lines(v1_lines, v2_lines, path)
    if diff:
        _write_lines(v2_lines, filename)
    return diff


def _apply_config_vals(config: RunConfig, keys: list[str], file_config: RunConfig):
    for key in keys:
        try:
            val = config[key]
        except KeyError:
            # key not provided in config, skip
            pass
        else:
            try:
                file_config[key] = val
            except KeyError:
                # key not in file config, skip
                pass


def _read_file_lines(filename: str):
    with open(filename) as f:
        return f.readlines()


def _applied_config_lines(config: RunConfig):
    return config.apply().splitlines(keepends=True)


def _diff_lines(lines1: list[str], lines2: list[str], path: str):
    return list(difflib.unified_diff(lines1, lines2, path, path))


def _write_lines(lines: list[str], filename: str):
    with open(filename, "w") as f:
        f.writelines(lines)
