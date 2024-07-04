# Op Version

The `op-version` sample project specifies an operation version.

    >>> use_project(sample("projects", "op-version"))

    >>> cat("gage.toml")
    [op]
    ⤶
    description = "Sample op"
    version = 1  # numeric val
    exec = "python op.py"
    ⤶
    [op-2]
    ⤶
    description = "Sample op 2"
    version = "2.0"  # string val
    exec = "python op.py"

Generate a run for `op`.

    >>> run("gage run op -qy")
    <0>

Version appears in the `show` command header after the operation name.
Gage prepends a lowercase 'v' to the version string.

    >>> run("gage show")  # +parse +panel
    {:run_id}
    | op-version:op v1                               completed |
    ⤶
    {}
    <0>

Version is stored in the run opref.

Get the run meta dir and load it.

    >>> run("gage select --meta-dir")  # +parse
    {meta_dir:path}
    <0>

    >>> from gage._internal.run_util import run_for_meta_dir

    >>> run1 = run_for_meta_dir(meta_dir)

Verify the operation version.

    >>> run1.opref
    <OpRef ns="op-version" name="op" version="1">

    >>> run1.opref.op_version
    '1'

Note that the version is a string value, even when specified as a number
in the Gage file (see above).

`op-2` defines a string version.

    >>> run("gage run op-2 -qy")
    <0>

    >>> run("gage show")  # +parse +panel
    {}
    | op-version:op-2 v2.0                           completed |
    {}
    <0>

    >>> run("gage select --meta-dir")  # +parse
    {meta_dir:path}
    <0>

    >>> run2 = run_for_meta_dir(meta_dir)

    >>> run2.opref
    <OpRef ns="op-version" name="op-2" version="2.0">

    >>> run2.opref.op_version
    '2.0'

## Use Version to Invalid Comparable Runs

The `version` run attribute can be used to invalid runs being considered
for being "comparable" to a request run. A comparable run causes a run
operation to be skipped when the `--needed` option is used with the
`run` command.

Create a sample project to illustrate.

    >>> use_project(make_temp_dir())

    >>> write("op.py", "")

    >>> write("gage.toml", """
    ... [op]
    ... exec = "python op.py"
    ... """)

Generate a run. We use `--needed` for each `run` command to illustrate
the "comparable" check behavior.

    >>> run("gage run op --needed -y")
    <0>

Attempt to run `op` again. In this case Gage skips the run because the
first run is considered comparable to what's being requested.

    >>> run("gage run op --needed -y")  # +parse
    Run skipped because a comparable run exists ({:run_name})
    ⤶
    For details, run 'gage show {:run_name}'
    <5>

The operation version is unset, which constitutes a value.

    >>> run("gage cat --meta --path opref")  # +parse
    2 {} op
    <0>

Set an operation version for `op` by updating the Gage file.

    >>> write("gage.toml", """
    ... [op]
    ... exec = "python op.py"
    ... version = "2"
    ... """)

Attempt to run `op` again. In this case, the requested operation version
is "2" and therefore there is no comparable run.

    >>> run("gage run op --needed -y")
    <0>

The operation version is reflected in the run.

    >>> run("gage cat --meta --path opref")  # +parse
    2 {} op 2
    <0>

Version is also now shown in the runs list.

    >>> run("gage runs -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | op v2     | completed |                              |
    | 2 | op        | completed |                              |
    <0>

When we try to run `op` again, Gage skips the run as it now has a
comparable run.

    >>> run("gage run op --needed -y")  # +parse
    Run skipped because a comparable run exists ({:run_name})
    ⤶
    For details, run 'gage show {:run_name}'
    <5>

Update the version again.

    >>> write("gage.toml", """
    ... [op]
    ... exec = "python op.py"
    ... version = "3"
    ... """)

    >>> run("gage run op --needed -y")
    <0>

    >>> run("gage runs -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | op v3     | completed |                              |
    | 2 | op v2     | completed |                              |
    | 3 | op        | completed |                              |
    <0>

    >>> run("gage run op --needed -y")  # +parse
    Run skipped because a comparable run exists ({:run_name})
    ⤶
    For details, run 'gage show {:run_name}'
    <5>
