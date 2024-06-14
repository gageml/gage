# Run when Needed

The `--needed` option tells Gage to look for a comparable run before
staging or running and skipping the operation if one is found.

A comparable run must meet these criteria:

- Match the operation (name and namespace)
- Not have an error status
- Match configuration

To illustrate, use the `config` sample project.

    >>> use_project(sample("projects", "config"))

The `config-types` operation accepts various config value types.

    >>> run("gage run config-types --help-op")
    Usage: gage run config-types
    ⤶
    ⤶
       Flags
    |                Default                                  |
    |  b             True                                     |
    |  f             4.567                                    |
    |  i             123                                      |
    |  s             Abc                                      |
    <0>

There are no runs to start.

    >>> run("gage runs -s")
    | # | operation | status | description                     |
    |---|-----------|--------|---------------------------------|
    <0>

Generate a run with default values.

    >>> run("gage run config-types -y")
    s=Abc i=123 f=4.567 b=True
    <0>

Get the run name.

    >>> run("gage select --name")  # +parse
    {run_name}
    <0>

Run the same operation with `--needed`.

    >>> run("gage run config-types --needed -y")  # +parse
    Run skipped because a comparable run exists ({x})
    ⤶
    For details, run 'gage show {y}'
    <5>

Note the error message refers to the comparable run using the run name.

    >>> assert x == y == run_name

Run the operation again with a different config value.

    >>> run("gage run config-types i=321 --needed -y")
    s=Abc i=321 f=4.567 b=True
    <0>

    >>> run("gage select --name")  # +parse
    {run_name}
    <0>

Run again with `--needed`.

    >>> run("gage run config-types i=321 --needed -y")  # +parse
    Run skipped because a comparable run exists ({x})
    ⤶
    For details, run 'gage show {y}'
    <5>

    >>> assert x == y == run_name

Run a different operation.

    >>> run("gage run hello-2 --needed -y")
    Hi Hola
    <0>

    >>> run("gage select --name")  # +parse
    {run_name}
    <0>

And again with `--needed`.

    >>> run("gage run hello-2 --needed -y")  # +parse
    Run skipped because a comparable run exists ({x})
    ⤶
    For details, run 'gage show {y}'
    <5>

    >>> assert x == y == run_name

## Run Status

A run must be a non-error status to be considered comparable.

Use the `run-errors` sample project.

    >>> use_project(sample("projects", "run-errors"))

Run `exec-error`. This fails.

    >>> run("gage run exec-error -y")
    <3>

    >>> run("gage runs -s")
    | # | operation  | status | description                    |
    |---|------------|--------|--------------------------------|
    | 1 | exec-error | error  |                                |
    <0>

Run again with `--needed`. In this case, Gage does not select the
previous run, even though it has the same name and same config values.

    >>> run("gage run exec-error --needed -y")
    <3>

    >>> run("gage runs -s")
    | # | operation  | status | description                    |
    |---|------------|--------|--------------------------------|
    | 1 | exec-error | error  |                                |
    | 2 | exec-error | error  |                                |
    <0>

## Run Namespace

Operations must be from the same namespace.

To illustrate, create two projects under the same parent directory.

    >>> parent_dir = make_temp_dir()

Project a:

    >>> project_a_dir = path_join(parent_dir, "a")
    >>> make_dir(project_a_dir)

    >>> write(path_join(project_a_dir, "gage.toml"), """
    ... [test]
    ... exec = "python -c \\"print('Hi from a')\\""
    ... """)

Project b:

    >>> project_b_dir = path_join(parent_dir, "b")
    >>> make_dir(project_b_dir)

    >>> write(path_join(project_b_dir, "gage.toml"), """
    ... [test]
    ... exec = "python -c \\"print('Hi from b')\\""
    ... """)

Store runs in the parent directory.

    >>> set_runs_dir(parent_dir)

Run operations from each project, storing the results in the parent
directory.

Project a:

    >>> cd(project_a_dir)

    >>> run("gage run test -y")
    Hi from a
    <0>

    >>> run("gage select --name")  # +parse
    {test_a_name}
    <0>

Project b:

    >>> cd(project_b_dir)

    >>> run("gage run test -y")
    Hi from b
    <0>

    >>> run("gage select --name")  # +parse
    {test_b_name}
    <0>

Show the runs.

    >>> cd(parent_dir)

    >>> run("gage runs -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | b:test    | completed |                              |
    | 2 | a:test    | completed |                              |
    <0>

Run the operation from each project again using `--needed`.

Project a:

    >>> cd(project_a_dir)

    >>> run("gage run test --needed -y")  # +parse
    Run skipped because a comparable run exists ({x})
    ⤶
    For details, run 'gage show {y}'
    <5>

Confirm that Gage identified the previous run from project a.

    >>> assert x == y == test_a_name

Project b:

    >>> cd(project_b_dir)

    >>> run("gage run test --needed -y")  # +parse
    Run skipped because a comparable run exists ({x})
    ⤶
    For details, run 'gage show {y}'
    <5>

Similarly, confirm that Gage identified the previous run for project b.

    >>> assert x == y == test_b_name

## Need with Batch

When a batch is specified, the batch applies the `--needed` option to
runs. If a run is not needed (i.e. there is a comparable run - see
above) it's skipped.

    >>> use_example("batch")

Run `hello` in a batch. Specify `--needed` to confirm that all batch
runs are generated.

We expect two runs, given the batch config.

    >>> cat("hello-batch.json")
    [
      { "name": "Cat", "n": 2 },
      { "name": "Dog", "n": 3 }
    ]

    >>> run("gage run hello --batch hello-batch.json --needed -y")
    Hello Cat 1
    Hello Cat 2
    Hello Dog 1
    Hello Dog 2
    Hello Dog 3
    <0>

    >>> run("gage ls -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | n=3 name=Dog                 |
    | 2 | hello     | completed | n=2 name=Cat                 |
    <0>

Run the command again.

    >>> run("gage run hello --batch hello-batch.json --needed -y")
    Skipped 2 runs because comparable runs exist
    <0>

Note that the exit code the batch is 0, unlike the exit code for skipped
run, which is 5 (see above). This is intentional, signifying that a
batch with `--needed` often expects comparable runs to exist and that
skipped runs are not an unexpected result.

    >>> run("gage ls -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | n=3 name=Dog                 |
    | 2 | hello     | completed | n=2 name=Cat                 |
    <0>
