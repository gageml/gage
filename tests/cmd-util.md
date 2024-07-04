# `util` command

The `util` command is a group of utility sub-commands.

    >>> run("gage util -h")  # +diff
    Usage: gage util [options] command
    ⤶
      Gage utility commands.
    ⤶
    Options:
      -h, --help  Show this message and exit.
    ⤶
    Commands:
      purge-run-files  Permanently delete run files.
    <0>

The command shows help by default.

    >>> run("gage util")  # +diff
    Usage: gage util [options] command
    ⤶
      Gage utility commands.
    ⤶
    Options:
      -h, --help  Show this message and exit.
    ⤶
    Commands:
      purge-run-files  Permanently delete run files.
    <0>
