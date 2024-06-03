# SPDX-License-Identifier: Apache-2.0

from typing import *

import json
import re

from .run_config import *
from .run_config_py import PythonConfig

__all__ = ["JupyterNotebookConfig"]


class JupyterNotebookConfig(RunConfig):
    def __init__(self, source: str):
        super().__init__()
        self._data = json.loads(source)
        _init_code_cell_config(self._data, self)
        self._initialized = True

    def apply(self):
        applied = dict(self._data)
        _apply_config(self, applied)
        return json.dumps(applied, indent=1) + "\n"


def _init_code_cell_config(data: dict[str, Any], config: RunConfig):
    for cell in data.get("cells", []):
        if cell.get("cell_type") == "code":
            lines = [_fix_magic_line(line) for line in cell.get("source", [])]
            source = "".join(lines)
            cell["__config"] = cell_config = PythonConfig(source)
            config.update(cell_config)


_INDENT_P = re.compile(r"(\s*)(.*)")


def _fix_magic_line(line: str):
    m = _INDENT_P.match(line)
    assert m, line
    ws, code = m.groups()
    if code.startswith("%"):
        return ws + _parse_magic_line(code) + "\n"
    return line


_MAGIC_P = re.compile(r"%([^\s]*)(?:\s+(.+))?")


def _parse_magic_line(code: str):
    m = _MAGIC_P.match(code)
    assert m, code
    name, arg = m.groups()
    return (
        f"get_ipython().run_line_magic({name!r}, {arg!r})"
        if arg
        else f"get_ipython().run_line_magic({name!r})"
    )


def _apply_config(config: RunConfig, data: dict[str, Any]):
    try:
        cells = data["cells"]
    except KeyError:
        pass
    else:
        data["cells"] = [_apply_cell_config(config, cell) for cell in cells]


def _apply_cell_config(config: RunConfig, cell: dict[str, Any]):
    try:
        cell_config = cell["__config"]
    except KeyError:
        return cell
    else:
        applied = dict(cell)
        del applied["__config"]
        applied["source"] = _applied_config_source(cell_config, config)
        return applied


def _applied_config_source(cell_config: RunConfig, config: RunConfig):
    for name in cell_config:
        cell_config[name] = config[name]
    lines = cell_config.apply().split("\n")
    return [line + "\n" for line in lines[:-1]] + [lines[-1]]
