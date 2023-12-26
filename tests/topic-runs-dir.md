# Runs Dir

These tests augment the runs dir related tests in
[`lib-var.md`](lib-var.md).

Gage looks for runs in the following locations, in order of priority:

1. Path specified by `GAGE_RUNS` env var
2. Path specified by `RUNS_DIR` env var
3. Path specified in `$runs-dir` in the active Gage file
4. `runs` subdir of the active project
5. `~/.gage/runs`

We the `check` command with the `-v / --verbose` option to verify runs
dir.

Create an empty project directory.

    >>> tmp = make_temp_dir()

Create a function to run the check command.

    >>> def check(env=None):
    ...     env = {"COLUMNS": "9999", **(env or {})}
    ...     run(f"gage -C '{tmp}' check -v", env=env)

Moving up the list of locations, the default runs dir is under the user
directory.

    >>> check()  # +parse -space
    {}
    | project_directory     | <none>                           |
    | gagefile              | <none>                           |
    | runs_directory        | {x:path}/.gage/runs              |
    <0>

    >>> user_home = os.path.expanduser("~")
    >>> assert x == user_home

Next, it's the `runs` subdirectory of the active project. To make `tmp`
a project, we can create a Gage file or any of the project markers
defined in `project_util.py`.

    >>> make_dir(path_join(tmp, ".vscode"))

    >>> check()  # +parse -space
    {}
    | project_directory     | {x:path}                         |
    | gagefile              | <none>                           |
    | runs_directory        | {y:path}/runs                    |
    <0>

    >>> assert x == y == tmp

We can configure a different location in a project Gage file.

    >>> write(path_join(tmp, "gage.yaml"), """
    ... $runs-dir: xyz
    ... """)

    >>> check()  # +parse -space
    {}
    | project_directory     | {x:path}                         |
    | gagefile              | {y:path}/gage.yaml               |
    | runs_directory        | {z:path}/xyz                     |
    <0>

    >>> assert x == y == z == tmp

Use `RUNS_DIR` to specify a location.

    >>> check({"RUNS_DIR": "xxx"})  # +parse -space
    {}
    | project_directory     | {x:path}                         |
    | gagefile              | {y:path}/gage.yaml               |
    | runs_directory        | xxx                              |
    <0>

    >>> assert x == y == tmp

`GAGE_RUNS` is used in priority of `RUNS_DIR` (e.g. in case there's
another application using `RUNS_DIR`).

    >>> check({"RUNS_DIR": "xxx", "GAGE_RUNS": "yyy"})  # +parse -space
    {}
    | project_directory     | {x:path}                         |
    | gagefile              | {y:path}/gage.yaml               |
    | runs_directory        | yyy                              |
    <0>

    >>> assert x == y == tmp

## Sample project

The `runs-dir` example project specifies `.runs` as the runs dir.

To test the project, copy it to a new location as we generate runs in
the project directory.

    >>> tmp = make_temp_dir()
    >>> copytree(example("runs-dir"), tmp)

    >>> cd(tmp)

    >>> ls(include_dirs=True)
    gage.yaml
    hello.py

    >>> run("gage ops")
    | operation | description |
    |-----------|-------------|
    | hello     | Says hello  |
    <0>

    >>> run("gage runs -s")
    | # | operation | status | label                           |
    |---|-----------|--------|---------------------------------|
    <0>

The Gage file defines `$runs-dir`.

    >>> cat("gage.yaml")
    $runs-dir: .runs
    ⤶
    hello:
      exec: python hello.py
      description: Says hello

Generate a run.

    >>> run("gage run hello -y")
    Hello!
    <0>

    >>> run("gage runs -s")  # +parse -space
    | # | operation | status | label |
    |-{}-|
    | 1 | gage-test-{}:hello | completed | |
    <0>

    >>> ls(include_dirs=True)  # +parse
    .runs
    .runs/{:run_id}
    .runs/{:run_id}.meta
    {}
    gage.yaml
    hello.py
