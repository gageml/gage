---
test-options: +paths
---

# Project reference

A project reference is a sidebar run file with a `.project` extension.
Gage creates a project reference to associate a run with its project.

Use the `hello` example for our initial tests.

    >>> use_example("hello")

Example runs are generated in a temporary directory, which is empty by
default.

    >>> from gage._internal.var import runs_dir

    >>> ls(runs_dir())
    <empty>

Generate a run.

    >>> run("gage run hello -y")
    Hello Gage
    <0>

Gage create a project reference to the example project.

    >>> ls(runs_dir())  # +parse
    {}
    {run_id:run_id}.project
    {}

    >>> cat(path_join(runs_dir(), f"{run_id}.project"))  # +parse
    file:{:path}/examples/hello

# Project relative paths

When a run is generated in a project subdirectory, Gage writes a
relative reference path.

Copy the hello example to a new directory.

    >>> tmp = make_temp_dir()
    >>> copytree(example("hello"), tmp)

Reset runs dir to use the default location for a project. We can use an
empty string.

    >>> set_runs_dir("")

Change to the new project location.

    >>> cd(tmp)

    >>> run("gage runs -s")
    | # | operation | status | label                           |
    |---|-----------|--------|---------------------------------|
    <0>

Run `hello` again.

    >>> run("gage run hello -y")
    Hello Gage
    <0>

    >>> run("gage runs -s")  # +parse -space
    | # | operation | status | label |
    |-{}-|
    | 1 | hello | completed | |
    <0>

Runs are created in the project `.gage/runs` subdirectory.

    >>> ls(".gage/runs")  # +parse
    {}
    {run_id:run_id}.project
    {}

The project reference uses a relative path.

    >>> cat(path_join(tmp, ".gage/runs", f"{run_id}.project"))
    file:../..
