# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

from .. import cli

from ..gagefile import gagefile_for_project

from . import error_handlers


def operations():
    try:
        table = operations_table()
    except GageFileError as e:
        error_handlers.gagefile_error(e)
    else:
        cli.out(table)


def operations_table():
    gf = gagefile_for_project()
    table = cli.Table("operation", "description")
    for name, opdef in sorted(gf.get_operations().items()):
        table.add_row(cli.label(name), _opdef_desc(opdef))
    return table


def _opdef_desc(opdef: OpDef):
    desc = _first_line(opdef.get_description() or "")
    return f"{desc} [dim](default)[/dim]" if opdef.get_default() else desc


def _first_line(s: str):
    return s.split("\n", 1)[0].strip()
