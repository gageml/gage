# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

from . import cli

from .file_select import parse_patterns
from .file_select import select_files

__all__ = [
    "init",
    "preview",
]

DEFAULT_PATTERNS = [
    "**/* text size<10000 max-matches=500",
    "-**/.* dir",
    "-**/* dir sentinel=bin/activate",
    "-**/* dir sentinel=.nocopy",
]


class RunSourceCode:
    def __init__(self, src_dir: str, patterns: list[str], paths: list[str]):
        self.src_dir = src_dir
        self.patterns = patterns
        self.paths = paths

    def as_json(self) -> dict[str, Any]:
        return {
            "src_dir": self.src_dir,
            "patterns": self.patterns,
            "paths": self.paths,
        }


def init(src_dir: str, opdef: OpDef):
    patterns = opdef_sourcecode_patterns(opdef)
    select = parse_patterns(patterns)
    paths = select_files(src_dir, select)
    return RunSourceCode(src_dir, patterns, paths)


def opdef_sourcecode_patterns(opdef: OpDef) -> list[str]:
    patterns = opdef.get_sourcecode()
    if patterns in (True, None):
        return DEFAULT_PATTERNS
    if not patterns:
        return []
    any_includes = any(not p.startswith("-") for p in patterns)
    if any_includes:
        return patterns
    return DEFAULT_PATTERNS + patterns


def preview(sourcecode: RunSourceCode):
    return cli.Panel(
        cli.Group(
            _preview_patterns_table(sourcecode.patterns),
            _preview_matched_files_table(sourcecode.paths),
        ),
        title="Source Code",
    )


def _preview_patterns_table(patterns: list[str]):
    table = cli.Table("patterns")
    if not patterns:
        table.add_row("[dim]<none>[/dim]")
    else:
        for pattern in patterns:
            table.add_row(pattern)
    return table


def _preview_matched_files_table(paths: list[str]):
    table = cli.Table("matched files")
    if not paths:
        table.add_row("[dim]<none>[/dim]")
    else:
        for path in paths:
            table.add_row(path)
    return table
