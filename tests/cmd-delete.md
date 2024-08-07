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

    >>> run("gage ls -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | name=Gage                    |
    <0>

Attempt to delete without specifying a run or `--all`.

    >>> run("gage rm -y")
    gage: Specify a run to delete or use '--all'.
    ⤶
    Use 'gage list' to show available runs.
    ⤶
    Try 'gage rm -h' for additional help.
    <1>

Attempt to delete a non-existing run.

    >>> run("gage rm 9z -y")
    gage: Nothing selected
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

    >>> run("gage ls -d -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | name=Gage                    |
    <0>

Generate another run.

    >>> run("gage run -l 'Run 2' -y")
    Hello Gage
    <0>

    >>> run("gage ls", cols=72)  # +parse +table
    | # | name  | operation | started | status    | description      |
    |---|-------|-----------|---------|-----------|------------------|
    | 1 | {nme} | hello     | {}      | completed | Run 2 name=Gage  |
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

    >>> run("gage list -d -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | name=Gage                    |
    <0>

## Where Filter

When `--where` is specified and runs aren't specified, Gage considers
`--all` to be implicitly specified.

Generate some runs.

    >>> run("gage run hello -l red name=A -qy")
    <0>
    >>> run("gage run hello -l red name=B -qy")
    <0>
    >>> run("gage run hello -l green name=C -qy")
    <0>
    >>> run("gage run hello -l green name=D -qy")
    <0>

    >>> run("gage ls -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | green name=D                 |
    | 2 | hello     | completed | green name=C                 |
    | 3 | hello     | completed | red name=B                   |
    | 4 | hello     | completed | red name=A                   |
    <0>

Guild complains when a run spec or `--all` isn't specified.

    >>> run("gage delete -y")
    gage: Specify a run to delete or use '--all'.
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

    >>> run("gage ls -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | green name=D                 |
    | 2 | hello     | completed | green name=C                 |
    <0>

Where can be used with run specs.

    >>> run("gage delete -w green 2 -y")
    Deleted 1 run
    <0>

    >>> run("gage ls -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | green name=D                 |
    <0>

## Errors

Can't use `--all` with run specs.

    >>> run("gage delete -a 123 -y")
    all and runs cannot be used together.
    ⤶
    Try 'gage delete -h' for help.
    <1>
