# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..cli import Argument
from ..cli import Option

OpSpec = Annotated[
    str,
    Argument(
        metavar="[operation]",
        help="Operation to run.",
    ),
]

FlagAssigns = Annotated[
    Optional[list[str]],
    Argument(
        metavar="[flag=value]...",
        help="Flag assignments.",
        show_default=False,
    ),
]

Label = Annotated[
    str,
    Option(
        "-l",
        "--label",
        metavar="label",
        help="Short label to describe the run.",
    ),
]

StageFlag = Annotated[
    bool,
    Option(
        "--stage",
        help="Stage a run but don't run it.",
        incompatible_with=["start"],
    ),
]

StartRun = Annotated[
    str,
    Option(
        "--start",
        metavar="run",
        help="Start a staged run. [arg]run[/] may be an ID, name, or index.",
        show_default=False,
    ),
]

Batch = Annotated[
    Optional[list[str]],
    Option(
        "-b",
        "--batch",
        metavar="filename",
        help="Run a batch.",
        show_default=False,
        incompatible_with=["start"],
    ),
]

MaxRuns = Annotated[
    int,
    Option(
        "-m",
        "--max-runs",
        metavar="N",
        help="Limit batch to N runs.",
        show_default=False,
    ),
]

QuietFlag = Annotated[
    bool,
    Option(
        "-q",
        "--quiet",
        help="Don't show output.",
    ),
]

YesFlag = Annotated[
    bool,
    Option(
        "-y",
        "--yes",
        help="Proceed without prompting.",
    ),
]

HelpOpFlag = Annotated[
    bool,
    Option(
        "--help-op",
        help="Show help for [arg]operation[/].",
    ),
]

PreviewFlag = Annotated[
    bool,
    Option(
        "--preview",
        help="Preview run steps without making changes.",
    ),
]

JSONFlag = Annotated[
    bool,
    Option(
        "--json",
        help="Output preview information as JSON.",
    ),
]


def run(
    opspec: OpSpec = "",
    flags: FlagAssigns = None,
    label: Label = "",
    stage: StageFlag = False,
    start: StartRun = "",
    batch: Batch = None,
    max_runs: MaxRuns = -1,
    quiet: QuietFlag = False,
    yes: YesFlag = False,
    help_op: HelpOpFlag = False,
    preview: PreviewFlag = False,
    json: JSONFlag = False,
):
    """Start or stage a run.

    [arg]operation[/] may be a file to run or an operation defined in a
    project Gage file. To list available options for the current
    directory, use '[cmd]gage operations[/]'.

    If [arg]operation[/] isn't specified, runs the default in the
    project Gage file.

    To run a batch, specify [arg]--batch[/] with a CSV or JSON formatted
    file specifying configuration for one or more runs. Try '[cmd]gage
    help batches[/]' for more information.
    """
    from .run_impl import run, Args

    run(
        Args(
            opspec,
            flags or [],
            label,
            stage,
            start,
            batch or [],
            max_runs,
            quiet,
            yes,
            help_op,
            preview,
            json,
        )
    )
