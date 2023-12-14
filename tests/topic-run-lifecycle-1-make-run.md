# Making a run

    >>> from gage._internal.run_util import *
    >>> from gage._internal.types import *

A run is created with `make_run()`. To create a run, specify:

- OpRef
- Optional location

If a location isn't specified, the run is assumed to be located in the
system default as provided by `sys_config.get_runs_home()`.

For the tests below, use a new location.

    >>> runs_home = make_temp_dir()

Create a new run.

    >>> opref = OpRef("test", "test")
    >>> run = make_run(opref, runs_home)

The run has a unique ID.

    >>> run.id  # +parse
    '{run_id:run_id}'

It has a name based on the ID.

    >>> run.name  # +parse
    '{run_name:run_name}'

    >>> assert run_name == run_name_for_id(run.id)

It has a corresponding run directory under `runs_home`.

    >>> run.run_dir  # +parse
    '{run_dir:path}'

The run directory is a subdirectory of the runs location and is named
with the run ID.

    >>> assert run_dir == path_join(runs_home, run_id)

The run directory doesn't exit.

    >>> assert not os.path.exists(run_dir)

The run has corresponding meta directory under `runs_home`.

    >>> run.meta_dir  # +parse
    '{meta_dir:path}'

The meta directory name is the same as the run directory plus ".meta".

    >>> assert meta_dir == run_dir + ".meta"

The meta directory does exist.

    >>> assert os.path.exists(meta_dir)

The meta directory contains a single file `opref`, which is the encoded
op ref for the run.

    >>> ls(meta_dir, include_dirs=True, permissions=True)
    -r--r--r-- opref

    >>> cat(path_join(meta_dir, "opref"))
    1 test test

The run status is `unknown`.

    >>> run_status(run)
    'unknown'
