# `purge` command

    >>> run("gage purge -h")  # +diff
    Usage: gage purge [options] [run]...
    ⤶
      Permanently delete runs.
    ⤶
      Use to remove deleted or archived runs, freeing disk
      space. Note that purged runs cannot be recovered.
    ⤶
      Use gage list --deleted to list deleted runs that can be
      removed.
    ⤶
      Use gage list --archive NAME to list archived runs that
      can be removed.
    ⤶
    Arguments:
      [run]...  Runs to remove. Required unless '--all' is
                specified.
    ⤶
    Options:
      -A, --archive name  Permanently delete archived runs.
      -w, --where expr    Select runs matching filter
                          expression.
      -a, --all           Remove all deleted runs.
      -y, --yes           Permanently delete runs without
                          prompting.
      -h, --help          Show this message and exit.
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
    | # | operation | status | description                     |
    |---|-----------|--------|---------------------------------|
    <0>

    >>> run("gage ls -s -d")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | name=Gage                    |
    <0>

Attempt to purge runs without specifying a run or `--all`.

    >>> run("gage purge -y")
    gage: Specify a deleted run to remove or use '--all'.
    ⤶
    Use 'gage list --deleted' to show deleted runs.
    ⤶
    Try 'gage purge -h' for additional help.
    <1>

Purge the run.

    >>> run("gage purge -a -y")
    Permanently deleted 1 run
    <0>

    >>> run("gage ls -s")
    | # | operation | status | description                     |
    |---|-----------|--------|---------------------------------|
    <0>

    >>> run("gage ls -s -d")
    | # | operation | status | description                     |
    |---|-----------|--------|---------------------------------|
    <0>

## Errors

Can't use `--all` with run spec.

    >>> run("gage purge -a 123 -y")
    all and runs cannot be used together.
    ⤶
    Try 'gage purge -h' for help.
    <1>
