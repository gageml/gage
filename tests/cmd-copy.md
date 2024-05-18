# `copy` Command

The `copy` command copies runs to and from external locations, including
other local directories.

Use the `hello` example to generate some test runs.

    >>> use_example("hello")

Generate test runs

    >>> run("gage run hello -l 'Run 1' -y")
    Hello Gage
    <0>

    >>> run("gage run hello name=Joe -l 'Run 2' -y")
    Hello Joe
    <0>

    >>> run("gage list -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | Run 2 name=Joe               |
    | 2 | hello     | completed | Run 1 name=Gage              |
    <0>

Create a directory to copy runs to.

    >>> tmp = make_temp_dir()

Copy all runs.

    >>> run(f"gage copy --all --dest {tmp} -y")
    Copied 2 runs
    <0>

    >>> run(f"gage --runs {tmp} runs -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | Run 2 name=Joe               |
    | 2 | hello     | completed | Run 1 name=Gage              |
    <0>

The `--sync` option syncs selected runs to the destination.

    >>> run(f"gage copy --all --where 'Run 1' --sync --dest {tmp} -y")
    Nothing copied, runs are up-to-date
    <0>

TODO: The output should reflect that there were deletions at the
destination.

    >>> run(f"gage --runs {tmp} runs -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | Run 1 name=Gage              |
    <0>

## Errors

Can't use `--all` with runs.

    >>> run("gage copy -a 123")
    all and runs cannot be used together.
    ⤶
    Try 'gage copy -h' for help.
    <1>
