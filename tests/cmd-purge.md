# `purge` command

    >>> run("gage purge -h")
    Usage: gage purge [options] [run]...
    ⤶
      Remove deleted runs.
    ⤶
      Use to remove deleted runs, freeing disk space. Note
      that purged runs cannot be recovered.
    ⤶
      Use gage list --deleted to list deleted runs that can be
      removed.
    ⤶
    Arguments:
      [run]...  Runs to remove. Required unless '--all' is
                specified.
    ⤶
    Options:
      -w, --where expr  Remove runs matching filter
                        expression.
      -a, --all         Remove all deleted runs.
      -y, --yes         Remove runs without prompting.
      -h, --help        Show this message and exit.
    <0>

Generate a run.

    >>> use_example("hello")

    >>> run("gage run -y")
    Hello Gage
    <0>

Delete the run.

    >>> run("gage delete -a -y")
    Deleted 1 run
    <0>

    >>> run("gage ls -s")
    | # | operation | status | label                           |
    |---|-----------|--------|---------------------------------|
    <0>

    >>> run("gage ls -s -d")
    | # | operation   | status    | label                      |
    |---|-------------|-----------|----------------------------|
    | 1 | hello:hello | completed |                            |
    <0>

Attempt to purge runs without specifying a run or `--all`.

    >>> run("gage purge -y")
    Specify a deleted run to remove or use '--all'.
    ⤶
    Use 'gage list --deleted' to show deleted runs.
    ⤶
    Try 'gage purge -h' for additional help.
    <1>

Purge the run.

    >>> run("gage purge -a -y")
    Permanently removed 1 run
    <0>

    >>> run("gage ls -s")
    | # | operation | status | label                           |
    |---|-----------|--------|---------------------------------|
    <0>

    >>> run("gage ls -s -d")
    | # | operation | status | label                           |
    |---|-----------|--------|---------------------------------|
    <0>
