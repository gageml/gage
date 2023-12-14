# SPDX-License-Identifier: Apache-2.0

from typing import *

import os

from .. import cli

from ..run_util import associate_project
from ..run_util import remove_associate_project

from .impl_support import one_run


class Args(NamedTuple):
    run: str
    project_dir: str | None
    remove: bool


def associate(args: Args):
    run = one_run(args)
    if args.remove:
        remove_associate_project(run)
        cli.out(f"Removed project association for \"{run.id}\"")
    else:
        if not args.project_dir:
            cli.exit_with_error(
                cli.markup(
                    "Argument 'project' is required unless '--remove' is used.\n\n"
                    "Try '[cmd]gage associate -h[/]' for help."
                )
            )
        abs_project_dir = os.path.abspath(args.project_dir)
        associate_project(run, os.path.abspath(abs_project_dir))
        cli.out(f"Associated \"{run.id}\" with {abs_project_dir}")
