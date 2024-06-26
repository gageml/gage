# SPDX-License-Identifier: Apache-2.0

from typing import *

from .. import cli

from ..run_attr import run_project_dir
from ..run_attr import run_user_attrs

from .impl_support import one_run_for_spec


class Args(NamedTuple):
    runs: list[str] | None
    name: bool
    run_dir: bool
    meta_dir: bool
    project_dir: bool
    label: bool


def select(args: Args):
    for spec in args.runs or [""]:
        run = one_run_for_spec(spec)
        if args.name:
            cli.out(run.name)
        elif args.run_dir:
            cli.out(run.run_dir)
        elif args.meta_dir:
            cli.out(run.meta_dir)
        elif args.project_dir:
            cli.out(run_project_dir(run) or "")
        elif args.label:
            cli.out(run_user_attrs(run).get("label") or "")
        else:
            cli.out(run.id)
