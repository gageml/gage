# `delete` command

    >>> run("gage delete -h")
    Usage: gage delete [options] [run]...
    ⤶
      Delete runs.
    ⤶
      run is either a run index, a run ID, or a run name.
      Partial values may be specified for run ID and run name
      if they uniquely identify a run. Multiple runs may be
      specified.
    ⤶
    Arguments:
      [run]...  Runs to delete. Required unless '--all' is
                specified.
    ⤶
    Options:
      -w, --where expr  Delete runs matching filter
                        expression.
      -a, --all         Delete all runs.
      -p, --permanent   Permanently delete runs. By default
                        deleted runs can be restored.
      -y, --yes         Delete runs without prompting.
      -h, --help        Show this message and exit.
    <0>

Generate a run.

    >>> use_example("hello")

    >>> run("gage run -y")
    Hello Gage
    <0>

    >>> run("gage ls -s")
    | # | operation | status    | label                        |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed |                              |
    <0>

Attempt to delete without specifying a run or `--all`.

    >>> run("gage rm -y")
    Specify a run to delete or use '--all'.
    ⤶
    Use 'gage list' to show available runs.
    ⤶
    Try 'gage rm -h' for additional help.
    <1>

Attempt to delete a non-existing run.

    >>> run("gage rm 9z -y")
    Nothing selected
    <1>

List deleted runs.

    >>> run("gage ls -d")
    | #   | name    | operation      | started     | status    |
    |-----|---------|----------------|-------------|-----------|
    <0>

Delete the run.

    >>> run("gage rm 1 -y")
    Deleted 1 run
    <0>

Show runs.

    >>> run("gage ls")
    | #   | name    | operation      | started     | status    |
    |-----|---------|----------------|-------------|-----------|
    <0>

Show deleted runs.

    >>> run("gage ls -ds")
    | # | operation | status    | label                        |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed |                              |
    <0>

Generate another run.

    >>> run("gage run -l 'Run 2' -y")
    Hello Gage
    <0>

    >>> run("gage ls", cols=64)  # +parse
    | # | name  | operation | started | status    | label          |
    |---|-------|-----------|---------|-----------|----------------|
    | 1 | {nme} | hello     | now     | completed | Run 2          |
    <0>

Permanently delete the run.

    >>> run(f"gage rm {nme} -p -y")
    Permanently deleted 1 run
    <0>

Show runs.

    >>> run("gage list")
    | #   | name    | operation      | started     | status    |
    |-----|---------|----------------|-------------|-----------|
    <0>

    >>> run("gage list -ds")
    | # | operation | status    | label                        |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed |                              |
    <0>

## Where Filter

When `--where` is specified and runs aren't specified, Gage considers
`--all` to be implicitly specified.

Generate some runs.

    >>> run("gage run hello -l red -qy")
    <0>
    >>> run("gage run hello -l red -qy")
    <0>
    >>> run("gage run hello -l green -qy")
    <0>
    >>> run("gage run hello -l green -qy")
    <0>

    >>> run("gage ls -s")
    | # | operation | status    | label                        |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | green                        |
    | 2 | hello     | completed | green                        |
    | 3 | hello     | completed | red                          |
    | 4 | hello     | completed | red                          |
    <0>

Guild complains when a run spec or `--all` isn't specified.

    >>> run("gage delete -y")
    Specify a run to delete or use '--all'.
    ⤶
    Use 'gage list' to show available runs.
    ⤶
    Try 'gage delete -h' for additional help.
    <1>

When `--where` is specified, Gage consider `--all` to be implicitly
specified, assuming the user wants to delete all matching runs.

    >>> run("gage delete -w red -y")
    Deleted 2 runs
    <0>

    >>> run("gage ls -s")
    | # | operation | status    | label                        |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | green                        |
    | 2 | hello     | completed | green                        |
    <0>

Where can be used with run specs.

    >>> run("gage delete -w green 2 -y")
    Deleted 1 run
    <0>

    >>> run("gage ls -s")
    | # | operation | status    | label                        |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | green                        |
    <0>
