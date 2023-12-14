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
    Arguments:
      [run]...  Runs to list. run may be a run ID, name, list
                index or slice.
    ⤶
    Options:
      -m, --more        Show more runs.
      -n, --limit max   Limit list to max runs.
      -a, --all         Show all runs. Cannot use with
                        --limit.
      -w, --where expr  Show runs matching filter expression.
      -d, --deleted     Show deleted runs.
      -h, --help        Show this message and exit.
    <0>

Generate some sample runs.

    >>> use_example("hello")

    >>> run("gage run hello -l run-1 -q -y")
    <0>

    >>> run("gage run hello -l run-2 -q -y")
    <0>

    >>> sleep(1)

    >>> run("gage run hello -l run-3 -q -y")
    <0>

List runs.

    >>> run("gage list -s")
    | # | operation   | status    | label                      |
    |---|-------------|-----------|----------------------------|
    | 1 | hello:hello | completed | run-3                      |
    | 2 | hello:hello | completed | run-2                      |
    | 3 | hello:hello | completed | run-1                      |
    <0>

## Incompatible params

    >>> run("gage list -n1 -a")
    all and limit cannot be used together.
    ⤶
    Try 'gage list -h' for help.
    <1>
