# `restore` command

    >>> run("gage restore -h")
    Usage: gage restore [options] [run]...
    ⤶
      Restore runs.
    ⤶
      Use to restore deleted runs. Note that is a run is
      permanently deleted, it cannot be restored.
    ⤶
      Use gage list --deleted to list deleted runs that can be
      restored.
    ⤶
    Arguments:
      [run]...  Runs to restore. Required unless '--all' is
                specified.
    ⤶
    Options:
      -w, --where expr  Restore runs matching filter
                        expression.
      -a, --all         Restore all deleted runs.
      -y, --yes         Restore runs without prompting.
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

Attempt to restore runs without specifying a run or `--all`.

    >>> run("gage restore -y")
    Specify a deleted run to restore or use '--all'.
    ⤶
    Use 'gage list --deleted' to show deleted runs.
    ⤶
    Try 'gage restore -h' for additional help.
    <1>

Restore the run.

    >>> run("gage restore -a -y")
    Restored 1 run
    <0>

    >>> run("gage ls -s")
    | # | operation   | status    | label                      |
    |---|-------------|-----------|----------------------------|
    | 1 | hello:hello | completed |                            |
    <0>

    >>> run("gage ls -s -d")
    | # | operation | status | label                           |
    |---|-----------|--------|---------------------------------|
    <0>
