# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option

RunSpec = Annotated[
    str,
    Argument(
        metavar="run",
        help=("Run to associate. Value may be an index number, run ID, or run name."),
        show_default=False,
    ),
]

ProjectDir = Annotated[
    Optional[str],
    Argument(
        metavar="[project]",
        help=(
            "Project directory to associate with [arg]run[/]. Required "
            "unless '--remove' is used."
        ),
        show_default=False,
    ),
]


RemoveFlag = Annotated[
    bool,
    Option(
        "-r",
        "--remove",
        help="Remove any associate project with [arg]run[/].",
    ),
]


def associate(run: RunSpec, project_dir: ProjectDir = None, remove: RemoveFlag = False):
    """Associate a run with a project directory.

    Local runs are associated with their projects by default. In cases
    where a run is imported from another system, use this command to
    link the run to a local project.

    To remove an associate, use the '--remove' option.
    """
    from .associate_impl import associate, Args

    associate(Args(run, project_dir, remove))
