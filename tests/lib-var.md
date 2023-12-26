# gage `var` support

    >>> from gage._internal.run_util import *
    >>> from gage._internal.types import *

The `var` module provides system wide data related services.

    >>> from gage._internal import var

Create a new runs home.

    >>> runs_dir = make_temp_dir()
    >>> set_runs_dir(runs_dir)

The initial list of runs is empty.

    >>> var.list_runs()
    []

Create a new run.

    >>> opref = OpRef("test", "test")
    >>> run = make_run(opref, runs_dir)

A run is represented by a `.meta` directory. The minimum definition of a
run is an `opref` file in the meta directory.

    >>> ls(runs_dir)  # +parse
    {id:run_id}.meta/opref

    >>> var.list_runs()  # +parse
    [<Run id="{x:run_id}" name="{:run_name}">]

By default, the run ID is the base name of the meta dir.

    >>> assert x == id

Run ID, run dir, and meta dir can be read from the run itself as
attributes.

    >>> run = var.list_runs()[0]

Run ID:

    >>> run.id  # +parse
    '{x:run_id}'

    >>> assert x == id

Run directory:

    >>> os.path.basename(run.run_dir)  # +parse
    '{x:run_id}'

    >>> assert x == id

    >>> assert run.run_dir == os.path.join(runs_dir, run.id)

Meta dir:

    >>> os.path.basename(run.meta_dir)  # +parse
    '{x:run_id}.meta'

    >>> assert x == id

    >>> assert run.meta_dir == os.path.join(runs_dir, run.id + ".meta")

Runs have other directories associated with them.

    >>> os.path.basename(run_project_ref(run))  # +parse
    '{x:run_id}.project'

    >>> assert x == id

    >>> os.path.basename(run_user_dir(run))  # +parse
    '{x:run_id}.user'

    >>> assert x == id

## Explicit run ID

An explicit run ID is specified by an `id` attribute file in the meta
directory.

    >>> write(os.path.join(runs_dir, id + ".meta", "id"), "abc")

    >>> var.list_runs()
    [<Run id="abc" name="babab-bopus">]

This attribute only effects the run ID. It does not change run paths.

    >>> run = var.list_runs()[0]

    >>> assert run.run_dir == os.path.join(runs_dir, id)
    >>> assert run.meta_dir == os.path.join(runs_dir, id + ".meta")

## Delete runs

Use `var.delete_runs` to delete one or more runs.

By default, runs are not permanently deleted. Each run directory is
renamed with a `.deleted` extension.

    >>> var.delete_runs(var.list_runs())
    [<Run id="abc" name="babab-bopus">]

    >>> ls(runs_dir)  # +parse
    {x:run_id}.meta.deleted/id
    {y:run_id}.meta.deleted/opref

    >>> assert x == y == id

Deleted runs aren't returned by `var.list_runs` by default.

    >>> var.list_runs()
    []

However, they are included if `deleted` is True.

    >>> var.list_runs(deleted=True)
    [<Run id="abc" name="babab-bopus">]

All run related paths end with '.deleted' to signify that the run is
deleted. The run ID itself does not change.

    >>> run = var.list_runs(deleted=True)[0]

Run ID:

    >>> run.id
    'abc'

Run dir:

    >>> os.path.basename(run.run_dir)  # +parse
    '{x:run_id}.deleted'

    >>> assert x == id

    >>> assert run.run_dir == os.path.join(runs_dir, id + ".deleted")

Run meta dir:

    >>> os.path.basename(run.meta_dir)  # +parse
    '{x:run_id}.meta.deleted'

    >>> assert x == id

    >>> assert run.meta_dir == os.path.join(runs_dir, id + ".meta.deleted")

Project ref:

    >>> os.path.basename(run_project_ref(run))  # +parse
    '{x:run_id}.project.deleted'

    >>> assert x == id

User dir:

    >>> os.path.basename(run_user_dir(run))  # +parse
    '{x:run_id}.user.deleted'

    >>> assert x == id

## Restore runs

Runs can be restored with `var.restore_runs`.

    >>> var.restore_runs(var.list_runs(deleted=True))
    [<Run id="abc" name="babab-bopus">]

    >>> ls(runs_dir) # +parse
    {x:run_id}.meta/id
    {y:run_id}.meta/opref

    >>> assert x == y == id

    >>> var.list_runs(deleted=True)
    []

    >>> var.list_runs()
    [<Run id="abc" name="babab-bopus">]

## Purging runs

Purging runs permanently deletes all associated files.

    >>> var.purge_runs(var.list_runs())
    [<Run id="abc" name="babab-bopus">]

    >>> ls(runs_dir)
    <empty>

## Canonical run dirs

A run is associated with multiple directories under runs home. These are
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

    >>> make_run(OpRef("test", "test"), runs_dir, "abc")
    <Run id="abc" name="babab-bopus">

    >>> ls(runs_dir, include_dirs=True)
    abc.meta
    abc.meta/opref

    >>> var.list_runs()
    [<Run id="abc" name="babab-bopus">]

The canonical directories:

    >>> run = var.list_runs()[0]

    >>> os.path.basename(run.run_dir)
    'abc'

    >>> os.path.basename(run.meta_dir)
    'abc.meta'

    >>> os.path.basename(run_user_dir(run))
    'abc.user'

    >>> os.path.basename(run_project_ref(run))
    'abc.project'

Create the remaining canonical directories.

    >>> make_dir(run.run_dir)
    >>> make_dir(run_user_dir(run))
    >>> touch(run_project_ref(run))

Create a non-canonical file.

    >>> touch(os.path.join(runs_dir, run.id + ".misc"))

Runs home contains the canonical list plus `<rundir>.misc`, which is not
considered part of the run.

    >>> ls(runs_dir, include_dirs=True)
    abc
    abc.meta
    abc.meta/opref
    abc.misc
    abc.project
    abc.user

Delete the run.

    >>> var.delete_runs([run])
    [<Run id="abc" name="babab-bopus">]

    >>> ls(runs_dir, include_dirs=True)
    abc.deleted
    abc.meta.deleted
    abc.meta.deleted/opref
    abc.misc
    abc.project.deleted
    abc.user.deleted

Note that `abc.misc` is not renamed.

Restore the run.

    >>> run = var.list_runs(deleted=True)[0]

    >>> var.restore_runs([run])
    [<Run id="abc" name="babab-bopus">]

    >>> ls(runs_dir, include_dirs=True)
    abc
    abc.meta
    abc.meta/opref
    abc.misc
    abc.project
    abc.user

## Runs home

`var` determines where runs are located. Use `runs_dir` to read the
current value.

We set runs home explicitly above using `set_homes_home`.

    >>> assert runs_dir == var.runs_dir()

This sets the environment variable `GAGE_RUNS`, which is the
authoritative method of setting runs home.

    >>> gage_runs_env = os.getenv("GAGE_RUNS")
    >>> assert runs_dir == gage_runs_env

If `GAGE_RUNS` is not set, `var` checks `RUNS_DIR`.

    >>> with Env({"GAGE_RUNS": "", "RUNS_DIR": "xyz"}):
    ...     var.runs_dir()
    'xyz'

If neither environment variable is set, `var` looks for a project
directory. See [`lib-project-util.md`](lib-project-util.md) for details
on how project directories are inferred.

Gage treats any directory with a Gage file as a project.

    >>> tmp = make_temp_dir()
    >>> touch(path_join(tmp, "gage.toml"))

When `runs_dir` is called in the context of a project directory, it
returns the `runs` subdirectory by default.

    >>> with Env({"GAGE_RUNS": ""}):  # +parse
    ...     with SetCwd(tmp):
    ...         var.runs_dir()
    '{x:path}/runs'

    >>> assert x == tmp

Finally, if the project specifies `$runs-dir`, it's used as a relative
path to the project directory.

    >>> write(path_join(tmp, "gage.toml"), """
    ... "$runs-dir" = "abc/xyz"
    ... """)

    >>> with Env({"GAGE_RUNS": ""}):  # +parse
    ...     with SetCwd(tmp):
    ...         var.runs_dir()
    '{x:path}/abc/xyz'

    >>> assert x == tmp

If the Gage file cannot be read, Gage logs a debug error message but
otherwise ignores the error and returns the default `runs` subdirectory
location.

    >>> write(path_join(tmp, "gage.toml"), """
    ... not a valid TOML file
    ... """)

    >>> with Env({"GAGE_RUNS": ""}):  # +parse
    ...     with SetCwd(tmp):
    ...         with LogCapture(log_level=0) as logs:
    ...             var.runs_dir()
    '{x:path}/runs'

    >>> assert x == tmp

    >>> logs.print_all()  # +parse -space
    DEBUG: [gage._internal.var] error reading Gage file in {x:path}:
    ('{y:path}/gage.toml', "Expected '=' after a key in a key/value
    pair (at line 2, column 5)")

    >>> assert x == y == tmp
