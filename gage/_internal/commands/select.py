# SPDX-License-Identifier: Apache-2.0

from typing import *

from typer import Argument
from typer import Option

RunSpecs = Annotated[
    Optional[list[str]],
    Argument(
        metavar="[run]...",
        help="Runs to select.",
        show_default=False,
    ),
]

NameFlag = Annotated[
    bool,
    Option(
        "--name",
        help="Select run name.",
    ),
]

RunDirFlag = Annotated[
    bool,
    Option(
        "--run-dir",
        help="Select run directory.",
    ),
]

MetaDirFlag = Annotated[
    bool,
    Option(
        "--meta-dir",
        help="Select meta directory.",
    ),
]

ProjectDirFlag = Annotated[
    bool,
    Option(
        "--project-dir",
        help="Select project directory.",
    ),
]

LabelFlag = Annotated[bool, Option("--label", help="Select run label.")]


def select(
    runs: RunSpecs = None,
    name: NameFlag = False,
    run_dir: RunDirFlag = False,
    meta_dir: MetaDirFlag = False,
    project_dir: ProjectDirFlag = False,
    label: LabelFlag = False,
):
    """Selects runs and their attributes.

    Prints the run ID for each selected run. Selects the latest run by
    default.
    """
    from .select_impl import select, Args

    select(
        Args(
            runs,
            name,
            run_dir,
            meta_dir,
            project_dir,
            label,
        )
    )
