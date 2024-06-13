# Run Utils - Meta Zip Support

    >>> from gage._internal.types import *
    >>> from gage._internal.run_util import *

Run meta may be defined in a directory or on a zip file. Either format
is informally referred to as "run meta".

`run_util` supports creating a zip file for meta via the private
function `_zip_meta`.

    >>> from gage._internal.run_util import _zip_meta

`_zip_meta` creates an zip archive of the meta directory and deletes the
meta directory.

To illustrate, create a temp directory to contain various sample meta
directories.

    >>> runs_dir = make_temp_dir()

Create a new run.

    >>> opref = OpRef("test", "test")
    >>> run = make_run(opref, runs_dir)

A run in this case consists only of the run meta directory, which
contains the `opref` file.

    >>> ls(runs_dir)  # +parse
    {run_id:run_id}.meta/opref

    >>> assert run_id == run.id

Use `var` to list the runs.

    >>> from gage._internal import var

    >>> var.list_runs(runs_dir)  # +parse
    [<Run id="{x}" name="{run_name:run_name}">]

    >>> assert x == run_id

Use `_zip_meta` to zip the meta directory for the run.

    >>> _zip_meta(run)

    >>> ls(runs_dir)  # +parse
    {x}.meta.zip

    >>> assert x == run_id

Use `var` to list the runs.

    >>> var.list_runs(runs_dir)  # +parse
    [<Run id="{x}" name="{y}">]

    >>> assert x == run_id

    >>> assert y == run_name
