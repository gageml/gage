# `run_meta` library

`run_meta` provides the interface to a run's meta data.

    >>> from gage._internal import run_meta

Run meta data can be stored either in a meta directory or in a zip file.
The interface supports writing to and read from the directory and
reading from the zip format.

For these tests, we use a runs located in a temp location.

    >>> runs_dir = make_temp_dir()

Create a function to initialize a new run.

    >>> from gage._internal.run_util import make_run as make_run0
    >>> from gage._internal.types import OpRef

    >>> def make_run(id=None):
    ...     return make_run0(OpRef("test", "test"), runs_dir, id)

Create a sample run with ID 'aaa'.

    >>> run = make_run("aaa")

    >>> ls(runs_dir)
    aaa.meta/opref

## Proc lock

The `proc/lock` file stores the run's current process ID (pid). If the
lock exists and contains an invalid pid, the run is considered to be
terminated. Otherwise the pid is used to monitor the run's process
status.

    >>> run_meta.write_proc_lock(run, 123)

    >>> ls(runs_dir)
    aaa.meta/opref
    aaa.meta/proc/lock

    >>> run_meta.read_proc_lock(run)
    '123'

TODO - test rest of run_meta functions.
