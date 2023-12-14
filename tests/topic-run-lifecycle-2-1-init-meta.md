# Initialing run meta

    >>> from gage._internal.run_util import *
    >>> from gage._internal.types import *

Run metadata ("meta") is information about the run that's independent of
the run directory. Meta is located in a side-car directory (i.e. located
along side the run directory) with the same name but ending in `.meta`.

Specifically excluded from the meta directory:

- Source code
- Dependencies
- Runtime files (e.g. virtual env, etc.)
- User-generated files

Initializing a run does not affect the run directory.

Run meta init is performed by `init_run_meta`. Meta must be initialized
before a run is staged or started.

Runs must exist before their meta directory is initialized. For details
on run creation, see [_Making a run_](topic-run-lifecycle-1-make-run.md).

Create runs in new runs home. A run requires an op ref.

    >>> runs_home = make_temp_dir()

    >>> opref = OpRef("test", "test")
    >>> run = make_run(opref, runs_home)

A run has an ID, a run directory, a meta directory, a reference to the
op ref, and a name.

    >>> run.id  # +parse
    '{:run_id}'

    >>> run.run_dir  # +parse
    '{:path}'

    >>> run.meta_dir  # +parse
    '{:path}.meta'

    >>> run.opref
    <OpRef ns="test" name="test">

The meta dir is created when the run is created.

    >>> assert os.path.exists(run.meta_dir)

The run directory, however, is not created.

    >>> assert not os.path.exists(run.run_dir)

The following is used to initialize a run:

- Op definition
- Op config
- Op command
- User attributes
- System attributes

Define inputs to the init function.

    >>> opdef = OpDef("test", {})

    >>> config = {
    ...     "x": 123,
    ...     "y": 1.23
    ... }

    >>> cmd = OpCmd(
    ...     ["echo", "hello"],
    ...     {"foo": "123", "bar": "abc"}
    ... )

    >>> system_attrs = {
    ...     "platform": "test 123"
    ... }

Initialize the run meta with `init_run_meta()`.

    >>> init_run_meta(
    ...     run,
    ...     opdef,
    ...     config,
    ...     cmd,
    ...     system_attrs
    ... )

Gage creates the following files:

    >>> ls(run.meta_dir, include_dirs=True, permissions=True)  # +diff
    -r--r--r-- __schema__
    -r--r--r-- config.json
    -r--r--r-- id
    -r--r--r-- initialized
    drwxrwxr-x log
    -rw-rw-r-- log/runner
    -r--r--r-- opdef.json
    -r--r--r-- opref
    drwxrwxr-x proc
    -r--r--r-- proc/cmd.json
    -r--r--r-- proc/env.json
    drwxrwxr-x sys
    -r--r--r-- sys/platform.json

Files are read only with the exception of the runner log, which is
assumed to be writable until the run is finalized (see below).

### `__schema__`

`__schema__` contains the schema used for the directory layout and
contents.

    >>> cat(run_meta_path(run, "__schema__"))  # +parse
    {x}

The current schema is defined by `META_SCHEMA`.

    >>> assert x == META_SCHEMA

### `config.json`

`config.json` is the JSON encoded run config. Run config is a flattened
namespace of keys and values. Keys are "dotted" to denote level
hierarchy.

    >>> cat(run_meta_path(run, "config.json"))
    {
      "x": 123,
      "y": 1.23
    }

### `id`

`id` is the run ID. This is saved in the meta dir for the contents to
remain independent of the container name.

    >>> cat(run_meta_path(run, "id"))  # +parse
    {x:run_id}

    >>> assert x == run.id

### `initialized`

`initialized` is a run timestamp that indicates when the run meta dir
was initialized. This is written at the end of the initialization
process.

    >>> cat(run_meta_path(run, "initialized"))  # +parse
    {:timestamp}

### `log/runner`

The runner log contains log entries for the actions performed.

Log entries are encoded in plain text, one per line. Lines are prefixed
with an ISO 8601 formatted date.

    >>> logfile = run_meta_path(run, "log", "runner")

    >>> cat(logfile)  # +parse
    {x:isodate} Writing meta id
    {}

    >>> assert datetime_fromiso(x) <= datetime_now()

The log contains a record of the changes made during init.

    >>> cat_log(logfile)  # +diff
    Writing meta id
    Writing meta opdef
    Writing meta config
    Writing meta proc cmd
    Writing meta proc env
    Writing meta sys/platform
    Writing meta initialized

Runner logs are not written with a log level. As a convention, messages
start with a capital letter.

### `opdef.json`

`opdef.json` is the JSON encoded operation definition, as provided by
either the project or otherwise generated for the run (e.g. dynamically
when running a language script).

This file is used when re-running the run or when using the run as a
prototype.

    >>> cat(run_meta_path(run, "opdef.json"))
    {}

### `opref`

`opref` is an encoded op reference. This is used in run listings to read
the run name efficiently.

    >>> cat(run_meta_path(run, "opref"))  # +parse
    1 test test

### `proc/cmd.json` and `proc/env.json`

`proc/cmd` and `proc/env` contain the run process command args and env
vars respectively. These are used to start the run process.

    >>> cat(run_meta_path(run, "proc", "cmd.json"))
    [
      "echo",
      "hello"
    ]

    >>> cat(run_meta_path(run, "proc", "env.json"))
    {
      "bar": "abc",
      "foo": "123"
    }

### System attributes

`sys` contains JSON encoded system attributes specified at the time of
meta init.

    >>> cat(run_meta_path(run, "sys", "platform.json"))
    "test 123"
