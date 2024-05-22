# Op Version

The `op-version` sample project specifies an operation version.

    >>> use_project(sample("projects", "op-version"))

    >>> cat("gage.toml")
    [op]
    ⤶
    description = "Sample op"
    version = "2"
    exec = "python op.py"

Generate a run for `op`.

    >>> run("gage run op -qy")
    <0>

Version appears in the `show` command header after the operation name.
Gage prepends a lowercase 'v' to the version string.

    >>> run("gage show")  # +parse +panel
    {:run_id}
    | op-version:op v2                               completed |
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
    <OpRef ns="op-version" name="op" version="2">

    >>> run1.opref.op_version
    '2'
