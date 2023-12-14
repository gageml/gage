# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import os

from . import sys_config

from .gagefile import gagefile_for_dir

from .project_util import find_project_dir

__all__ = [
    "resolve_run_context",
]


def resolve_run_context(opspec: str, command_dir: str | None = None):
    command_dir = command_dir or sys_config.cwd()
    project_dir = find_project_dir(command_dir) or command_dir
    gf = gagefile_for_dir(project_dir)
    namespace = _namespace(gf, project_dir)
    opdef = _opdef_for_spec(opspec, gf)
    return RunContext(
        command_dir=command_dir,
        project_dir=project_dir,
        gagefile=gf,
        opref=OpRef(namespace, opdef.name),
        opdef=opdef,
    )


def _namespace(gagefile: GageFile, project_dir: str):
    project_dir = os.path.realpath(project_dir)
    project_basename = os.path.basename(project_dir)
    assert project_basename, project_dir
    return project_basename


def _opdef_for_spec(spec: str, gagefile: GageFile):
    if spec:
        return _opdef_for_name(spec, gagefile)
    else:
        return _default_opdef(gagefile)


def _opdef_for_name(name: str, gagefile: GageFile):
    try:
        return gagefile.get_operations()[name]
    except KeyError:
        raise OpDefNotFound(name, gagefile.filename)


def _default_opdef(gagefile: GageFile):
    for name, opdef in sorted(gagefile.get_operations().items()):
        if opdef.get_default:
            return opdef
    raise OpDefNotFound(None, gagefile.filename)
