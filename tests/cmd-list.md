# `list` command

`list` shows runs.

    >>> run("gage list -h")  # +diff
    Usage: gage list [options] [run]...
    ⤶
      List runs.
    ⤶
      By default the latest 20 runs are shown. To show more
      runs, use '-m / --more' or '-n / --limit'. Use '-a /
      --all' to show all runs.
    ⤶
      Use '-w / --where' to filter runs. Try 'gage help
      filters' for help with filter expressions.
    ⤶
      Runs may be selected from the list using run IDs, names,
      indexes or slice notation. Try 'gage help select-runs'
      for help with select options.
    ⤶
      To show deleted runs, use '--deleted',
    ⤶
      To show archives runs, use '--archive name'. Use 'gage
      archive --list' for a list of archive names.
    ⤶
    Arguments:
      [run]...  Runs to list. run may be a run ID, name, list
                index or slice.
    ⤶
    Options:
      -m, --more          Show more runs.
      -n, --limit max     Limit list to max runs.
      -a, --all           Show all runs. Cannot use with
                          --limit.
      -w, --where expr    Show runs matching filter
                          expression.
      -d, --deleted       Show deleted runs.
      -A, --archive name  Show archived runs.
      -h, --help          Show this message and exit.
    <0>

Generate some sample runs.

    >>> use_example("hello")

    >>> run("gage run hello name=Red -l run-1 -q -y")
    <0>

    >>> run("gage run hello name=Green -l run-2 -q -y")
    <0>

    >>> sleep(1)

    >>> run("gage run hello name=Blue -l run-3 -q -y")
    <0>

List runs.

    >>> run("gage list -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | run-3 name=Blue              |
    | 2 | hello     | completed | run-2 name=Green             |
    | 3 | hello     | completed | run-1 name=Red               |
    <0>

## Incompatible params

    >>> run("gage list -n1 -a")
    limit and all cannot be used together.
    ⤶
    Try 'gage list -h' for help.
    <1>
