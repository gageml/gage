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

## Move Runs

TODO - tell the delete / archive and restore story

`run_move` provides support for run moves.

    >> from gage._internal.run_move import *

A run move is an action that moves a run from one virtual container to
another.

A virtual container may be one of:

- active
- trash
- an archive

Active runs are listed by running `gage runs`.

Runs in `trash` are deleted and are listed by running `gage runs
--deleted`.

Runs located in an archive appear are listed by running `gage runs
--archive <name>`.

Runs are physically located together in a single directory regardless of
the virtual container they're in.

Create a directory to store runs in.

    >> runs_dir = make_temp_dir()

Create a run.

    >> from gage._internal.types import *
    >> from gage._internal.run_util import make_run

    >> run = make_run(OpRef("test", "test"), runs_dir, "aaa")

    >> ls(runs_dir)
    aaa.meta/opref


--------------------------------------------------------------------------
OLD TESTS
--------------------------------------------------------------------------

    >> from gage._internal.run_attr import *
    >> from gage._internal.run_util import *
    >> from gage._internal.types import *

The `var` module provides system wide data related services.

    >> from gage._internal import var

Create a new runs dir.

    >> runs_dir = make_temp_dir()
    >> set_runs_dir(runs_dir)

The initial list of runs is empty.

    >> var.list_runs()
    []

Create a new run.

    >> opref = OpRef("test", "test")
    >> run = make_run(opref, runs_dir)

A run is represented by a `.meta` directory. The minimum definition of a
run is an `opref` file in the meta directory.

    >> ls(runs_dir)  # +parse
    {id:run_id}.meta/opref

    >> var.list_runs()  # +parse
    [<Run id="{x:run_id}" name="{:run_name}">]

By default, the run ID is the base name of the meta dir.

    >> assert x == id

Run ID, run dir, and meta dir can be read from the run itself as
attributes.

    >> run = var.list_runs()[0]

Run ID:

    >> run.id  # +parse
    '{x:run_id}'

    >> assert x == id

Run directory:

    >> os.path.basename(run.run_dir)  # +parse
    '{x:run_id}'

    >> assert x == id

    >> assert run.run_dir == os.path.join(runs_dir, run.id)

Meta dir:

    >> os.path.basename(run.meta_dir)  # +parse
    '{x:run_id}.meta'

    >> assert x == id

    >> assert run.meta_dir == os.path.join(runs_dir, run.id + ".meta")

Runs have other directories associated with them.

    >> os.path.basename(run_project_ref(run))  # +parse
    '{x:run_id}.project'

    >> assert x == id

    >> os.path.basename(run_user_dir(run))  # +parse
    '{x:run_id}.user'

    >> assert x == id

## Explicit run ID

An explicit run ID is specified by an `id` attribute file in the meta
directory.

    >> write(os.path.join(runs_dir, id + ".meta", "id"), "abc")

    >> var.list_runs()
    [<Run id="abc" name="babab-bopus">]

This attribute only effects the run ID. It does not change run paths.

    >> run = var.list_runs()[0]

    >> assert run.run_dir == os.path.join(runs_dir, id)
    >> assert run.meta_dir == os.path.join(runs_dir, id + ".meta")

## Delete runs

Use `var.delete_runs` to delete one or more runs.

By default, runs are not permanently deleted. Each run directory is
renamed with a `.deleted` extension.

    >> var.delete_runs(var.list_runs())
    [<Run id="abc" name="babab-bopus">]

    >> ls(runs_dir)  # +parse
    {x:run_id}.meta.deleted/id
    {y:run_id}.meta.deleted/opref

    >> assert x == y == id

Deleted runs aren't returned by `var.list_runs` by default.

    >> var.list_runs()
    []

However, they are included if `deleted` is True.

    >> var.list_runs(deleted=True)
    [<Run id="abc" name="babab-bopus">]

All run related paths end with '.deleted' to signify that the run is
deleted. The run ID itself does not change.

    >> run = var.list_runs(deleted=True)[0]

Run ID:

    >> run.id
    'abc'

Run dir:

    >> os.path.basename(run.run_dir)  # +parse
    '{x:run_id}.deleted'

    >> assert x == id

    >> assert run.run_dir == os.path.join(runs_dir, id + ".deleted")

Run meta dir:

    >> os.path.basename(run.meta_dir)  # +parse
    '{x:run_id}.meta.deleted'

    >> assert x == id

    >> assert run.meta_dir == os.path.join(runs_dir, id + ".meta.deleted")

Project ref:

    >> os.path.basename(run_project_ref(run))  # +parse
    '{x:run_id}.project.deleted'

    >> assert x == id

User dir:

    >> os.path.basename(run_user_dir(run))  # +parse
    '{x:run_id}.user.deleted'

    >> assert x == id

## Restore runs

Runs can be restored with `var.restore_runs`.

    >> var.restore_runs(var.list_runs(deleted=True))
    [<Run id="abc" name="babab-bopus">]

    >> ls(runs_dir) # +parse
    {x:run_id}.meta/id
    {y:run_id}.meta/opref

    >> assert x == y == id

    >> var.list_runs(deleted=True)
    []

    >> var.list_runs()
    [<Run id="abc" name="babab-bopus">]

## Purging runs

Purging runs permanently deletes all associated files.

    >> var.purge_runs(var.list_runs())
    [<Run id="abc" name="babab-bopus">]

    >> ls(runs_dir)
    <empty>

## Canonical run dirs

A run is associated with multiple directories under runs dir. These are
the canonical run directories.

| Dir                | Purpose                                            |
| ------------------ | -------------------------------------------------- |
| `<rundir>`         | User files (source code, runtime, deps, generated) |
| `<rundir>.meta`    | Gage-written files (attrs, logs, summaries)        |
| `<rundir>.user`    | User attributes                                    |
| `<rundir>.project` | Project reference                                  |

Deleted runs mirror this structure where each directory ends with
`.deleted`.

Create a new run.

    >> make_run(OpRef("test", "test"), runs_dir, "abc")
    <Run id="abc" name="babab-bopus">

    >> ls(runs_dir, include_dirs=True)
    abc.meta
    abc.meta/opref

    >> var.list_runs()
    [<Run id="abc" name="babab-bopus">]

The canonical directories:

    >> run = var.list_runs()[0]

    >> os.path.basename(run.run_dir)
    'abc'

    >> os.path.basename(run.meta_dir)
    'abc.meta'

    >> os.path.basename(run_user_dir(run))
    'abc.user'

    >> os.path.basename(run_project_ref(run))
    'abc.project'

Create the remaining canonical directories.

    >> make_dir(run.run_dir)
    >> make_dir(run_user_dir(run))
    >> touch(run_project_ref(run))

Create a non-canonical file.

    >> touch(os.path.join(runs_dir, run.id + ".misc"))

Runs dir contains the canonical list plus `<rundir>.misc`, which is not
considered part of the run.

    >> ls(runs_dir, include_dirs=True)
    abc
    abc.meta
    abc.meta/opref
    abc.misc
    abc.project
    abc.user

Delete the run.

    >> var.delete_runs([run])
    [<Run id="abc" name="babab-bopus">]

    >> ls(runs_dir, include_dirs=True)
    abc.deleted
    abc.meta.deleted
    abc.meta.deleted/opref
    abc.misc
    abc.project.deleted
    abc.user.deleted

Note that `abc.misc` is not renamed.

Restore the run.

    >> run = var.list_runs(deleted=True)[0]

    >> var.restore_runs([run])
    [<Run id="abc" name="babab-bopus">]

    >> ls(runs_dir, include_dirs=True)
    abc
    abc.meta
    abc.meta/opref
    abc.misc
    abc.project
    abc.user
