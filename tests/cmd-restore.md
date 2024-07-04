# `restore` command

    >>> run("gage restore -h")  # +diff
    Usage: gage restore [options] [run]...
    ⤶
      Restore deleted or archived runs.
    ⤶
      Use to restore deleted runs. Note that is a run is
      permanently deleted, it cannot be restored.
    ⤶
      Use gage list --deleted to list deleted runs that can be
      restored.
    ⤶
      If '--archive' is specified, restores runs from the
      archive. Use gage archive --list for a list of archive
      names.
    ⤶
    Arguments:
      [run]...  Runs to restore. Required unless '--all' is
                specified.
    ⤶
    Options:
      -w, --where expr    Restore runs matching filter
                          expression.
      -A, --archive name  Restore runs from an archive.
      -a, --all           Restore all runs.
      -y, --yes           Restore runs without prompting.
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

    >>> run("gage ls -0")
    | # | operation | status | description                     |
    |---|-----------|--------|---------------------------------|
    <0>

    >>> run("gage ls -d -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | name=Gage                    |
    <0>

Attempt to restore runs without specifying a run or `--all`.

    >>> run("gage restore -y")
    gage: Specify a run to restore or use '--all'.
    ⤶
    Use 'gage list --deleted' to show deleted runs.
    ⤶
    Try 'gage restore -h' for additional help.
    <1>

Restore the run.

    >>> run("gage restore -a -y")
    Restored 1 run
    <0>

    >>> run("gage ls -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | name=Gage                    |
    <0>

    >>> run("gage ls -d -0")
    | # | operation | status | description                     |
    |---|-----------|--------|---------------------------------|
    <0>

## Errors

Can't use `--all` with runs.

    >>> run("gage restore -a 123 -y")
    all and runs cannot be used together.
    ⤶
    Try 'gage restore -h' for help.
    <1>
