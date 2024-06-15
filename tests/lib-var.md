---
test-options: +paths
---

# gage `var` support

The `var` module provides system wide data related services.

    >>> from gage._internal import var

- Resolve runs directory
- List runs
- Delete runs
- Archive runs
- Restore runs

## Runs Dir

Runs are stored in a run directory. Use `runs_dir()` to get that
directory according to the current configuration.

The runs directory is resolved using the following rules, each applied
in order:

- Location specified by the `GAGE_RUNS` environment variable
- Location specified by the `RUNS_DIR` environment variable
- Location relative to the current project
- System wide location

### Runs Dir Environment Variables

Gage uses environment variables to explicitly set the runs directory.
These values are used as provided.

Two environment variables are supported: first `GAGE_RUNS` and then
`RUNS_DIR`.

    >>> with Env({"GAGE_RUNS": "abc", "RUNS_DIR": "xyz"}):
    ...     var.runs_dir()
    'abc'

    >>> with Env({"GAGE_RUNS": "", "RUNS_DIR": "xyz"}):
    ...     var.runs_dir()
    'xyz'

### Project Relative Runs Dir

If neither environment variable is set, `var` looks for a project
directory. See [`lib-project-util.md`](lib-project-util.md) for details
on how Gage resolves project directories.

Gage treats any directory with a Gage file as a project.

    >>> from gage._internal.project_util import has_project_marker

    >>> project_dir = make_temp_dir()
    >>> has_project_marker(project_dir)
    False

    >>> touch(path_join(project_dir, "gage.toml"))
    >>> has_project_marker(project_dir)
    True

`runs_dir`, when called in the context of a project directory, returns
the `.gage/runs` subdirectory by default.

Note, we reset the environment to ensure that we don't use environment
inadvertently.

    >>> with Env({}, replace=True):  # +parse
    ...     with SetCwd(project_dir):
    ...         var.runs_dir()
    '{x:path}/.gage/runs'

    >>> compare_paths(x, project_dir)
    True

If the project is configured to use a specific runs directory,
`runs_dir()` returns that location. An explicit runs directory is
configured by setting the `$runs-dir` project attribute.

    >>> write(path_join(project_dir, "gage.toml"), """
    ... "$runs-dir" = "abc/xyz"
    ... """)

    >>> with Env({}, replace=True):  # +parse
    ...     with SetCwd(project_dir):
    ...         var.runs_dir()
    '{x:path}/abc/xyz'

    >>> compare_paths(x, project_dir)
    True

If Gage can't read the project configuration, it logs a debug error
message and returns the default `.gage/runs` subdirectory location.

    >>> write(path_join(project_dir, "gage.toml"), """
    ... not a valid TOML file
    ... """)

    >>> with Env({}, replace=True):  # +parse
    ...     with SetCwd(project_dir):
    ...         with LogCapture(log_level=0) as logs:
    ...             var.runs_dir()
    '{x:path}/.gage/runs'

    >>> compare_paths(x, project_dir)
    True

    >>> logs.print_all()  # +parse -space
    DEBUG: [gage._internal.var] error reading Gage file in {x:path}:
    ('{y:path}/gage.toml', "Expected '=' after a key in a key/value
    pair (at line 2, column 5)")

    >>> compare_paths(x, y) and compare_paths(y, project_dir)
    True

### System Level Runs Dir

If Gage cannot resolve the runs directory through environment variables
or relative to a project, it defaults to a system level location.

The system level location is `~/.gage/runs` where `~` is the current
user home directory.

    >>> with Env({}, replace=True):  # +parse
    ...     with SetCwd("/"):
    ...         var.runs_dir()
    '{x:path}/.gage/runs'

    >>> compare_paths(x, os.path.expanduser("~"))
    True

## List Runs

Use `list_runs()` to return a list of runs for the current runs
directory.

Create a sample runs directory.

    >>> runs_dir = make_temp_dir()

The list is empty.

    >>> with Env({"GAGE_RUNS": runs_dir}):
    ...     var.list_runs()
    []

Create a run.

    >>> from gage._internal.types import OpRef
    >>> from gage._internal.run_util import make_run

    >>> make_run(OpRef("test", "test"), runs_dir, "aaa")
    <Run id="aaa" name="babab-bopop">

List the runs.

    >>> with Env({"GAGE_RUNS": runs_dir}):
    ...     var.list_runs()
    [<Run id="aaa" name="babab-bopop">]

`list_runs()` accepts an explicit runs directory (root).

    >>> var.list_runs(runs_dir)
    [<Run id="aaa" name="babab-bopop">]

Run meta and run dirs are associated with the runs dir they're located
in.

    >>> run = var.list_runs(runs_dir)[0]

    >>> run.id
    'aaa'

    >>> run.run_dir  # +parse
    '{x:path}/aaa'

    >>> run.meta_dir  # +parse
    '{y:path}/aaa.meta'

    >>> compare_paths(x, runs_dir)
    True

    >>> compare_paths(y, runs_dir)
    True

### Filter Runs

The result from `list_run()` can be filtered using a run filter. A run
filter is a function that accepts a single run as an argument and
returns a boolean indicating whether or not the include the run in the
list.

Create two more runs in `runs_dir`.

    >>> make_run(OpRef("test", "test"), runs_dir, "bbb")
    <Run id="bbb" name="babab-bovur">

    >>> make_run(OpRef("test", "test"), runs_dir, "ccc")
    <Run id="ccc" name="babab-bugas">

List runs whose ID starts with "a":

    >>> var.list_runs(runs_dir, filter=lambda run: run.id[:1] == "a")
    [<Run id="aaa" name="babab-bopop">]

### Sort Runs

Results from `list_runs()` can be sorted by specifying a list of sort
rules. By default, result sort order is unspecified and can't be relied
on for deterministic behavior.

Order runs by ID in ascending order:

    >>> var.list_runs(runs_dir, sort=["id"])  # -space
    [<Run id="aaa" name="babab-bopop">,
     <Run id="bbb" name="babab-bovur">,
     <Run id="ccc" name="babab-bugas">]

Order runs by ID in descending order:

    >>> var.list_runs(runs_dir, sort=["-id"])  # -space
    [<Run id="ccc" name="babab-bugas">,
     <Run id="bbb" name="babab-bovur">,
     <Run id="aaa" name="babab-bopop">]

## Move Runs

Use `move_runs()` or `move_run()` to move runs within a run directory to
virtual containers.

Run moves do not change the file system location of runs.

Run moves are used to non-permanently delete runs, archive runs, and to
restore runs.

Create a new runs directory.

    >>> runs_dir = make_temp_dir()

Create a sample run.

    >>> run = make_run(OpRef("test", "test"), runs_dir, "aaa")

    >>> ls(runs_dir)
    aaa.meta/opref

    >>> var.list_runs(runs_dir)
    [<Run id="aaa" name="babab-bopop">]

Use the `run_move` module to get information about a run's current
container.

    >>> from gage._internal.run_move import run_container

    >>> run_container(run)  # +pprint
    'active'

The "active" container is the default container used for lists. The name
of the active container is defined in `run_move.ACTIVE_CONTAINER`.

    >>> from gage._internal.run_move import ACTIVE_CONTAINER

    >>> ACTIVE_CONTAINER
    'active'

Move the run to the "trash" container.

    >>> var.move_run(run, var.TRASH)

    >>> run_container(run)
    'trash'

The run is not returned by `list_runs()`.

    >>> var.list_runs(runs_dir)
    []

It is returned when we specify its container.

    >>> var.list_runs(runs_dir, container=var.TRASH)
    [<Run id="aaa" name="babab-bopop">]

Containers are designated by markers, which are located in a `.move`
sidecar directory.

    >>> ls(runs_dir)  # +parse
    aaa.meta/opref
    aaa.move/{:uuid4}-trash

Restore the run using `restore_run()`. This is equivalent of moving the
run to the "active" container.

    >>> var.restore_run(run)

    >>> run_container(run)
    'active'

This removes the run from the "trash" container.

    >>> var.list_runs(runs_dir, container=var.TRASH)
    []

The run appears in the default list.

    >>> var.list_runs(runs_dir)
    [<Run id="aaa" name="babab-bopop">]

Move markers are added to the run move directory. The latest marker
indicates the run's container.

    >>> ls(runs_dir, natsort=False)  # +parse
    aaa.meta/opref
    aaa.move/{:uuid4}-trash
    aaa.move/{:uuid4}-active

This scheme is used to support move operations across systems with the
ability to merge moves to resolve a final location (last time based move
wins).

Moves are used to archive runs.

    >>> var.move_run(run, "archive-1")

    >>> run_container(run)
    'archive-1'

    >>> var.list_runs(runs_dir)
    []

    >>> var.list_runs(runs_dir, container="archive-1")
    [<Run id="aaa" name="babab-bopop">]

Archived runs can be moved to trash.

    >>> var.move_run(run, var.TRASH)

    >>> var.list_runs(runs_dir, container="archive-1")
    []

    >>> var.list_runs(runs_dir, container="trash")
    [<Run id="aaa" name="babab-bopop">]

And finally restore. In this case we move the run to the "active"
container, which is the equivalent to restoring.

    >>> var.move_run(run, container=var.ACTIVE)

    >>> var.list_runs(runs_dir, container="trash")
    []

    >>> var.list_runs(runs_dir)
    [<Run id="aaa" name="babab-bopop">]

    >>> var.list_runs(runs_dir, container=var.ACTIVE)
    [<Run id="aaa" name="babab-bopop">]

## Delete runs

`delete_runs()` and `delete_run()` permanently deletes runs and their
associated sidecar directories.

Create a new runs directory.

    >>> runs_dir = make_temp_dir()

Create a run.

    >>> run = make_run(OpRef("test", "test"), runs_dir, "bbb")

    >>> var.list_runs(runs_dir)
    [<Run id="bbb" name="babab-bovur">]

The run is defined by a single `opref` file in a meta directory.

    >>> ls(runs_dir)
    bbb.meta/opref

Delete the run.

    >>> var.delete_run(run)

    >>> var.list_runs(runs_dir)
    []

    >>> ls(runs_dir)
    <empty>

Create another runs and include some sidecar files.

    >>> run = make_run(OpRef("test", "test"), runs_dir, "ccc")

    >>> make_dir(path_join(runs_dir, "ccc"))
    >>> touch(path_join(runs_dir, "ccc", "eval.py"))
    >>> touch(path_join(runs_dir, "ccc.abc"))
    >>> touch(path_join(runs_dir, "ccc.xyz"))

Move the run to generate some move markers.

    >>> var.move_run(run, "archive-1")
    >>> var.move_run(run, "active")

Show the run files.

    >>> ls(runs_dir, natsort=False)  # +parse
    ccc.abc
    ccc.meta/opref
    ccc.move/{:uuid4}-archive-1
    ccc.move/{:uuid4}-active
    ccc.xyz
    ccc/eval.py

Delete the run.

    >>> var.delete_run(run)

    >>> ls(runs_dir)
    <empty>

A deleted can't be moved.

    >>> var.move_run(run, var.TRASH)  # +parse
    Traceback (most recent call last):
    FileNotFoundError: {}/ccc.meta
