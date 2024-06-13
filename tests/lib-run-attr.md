# Run attribute support

Run attribute support is provided by `run_attr`.

    >>> from gage._internal.types import *
    >>> from gage._internal.run_attr import *
    >>> from gage._internal.run_util import *

## Core attributes

Core attributes supported `run_attr()` are listed in the private module
variable `_ATTR_READERS`.

    >>> from gage._internal.run_attr import _ATTR_READERS

    >>> sorted(_ATTR_READERS)  # +pprint +diff
    ['dir',
     'exit_code',
     'id',
     'name',
     'staged',
     'started',
     'stopped',
     'timestamp']

Create a run that's not bound to a directory.

    >>> run = Run(
    ...     id="abc",
    ...     opref=OpRef("test", "test"),
    ...     meta_dir="/meta_dir",
    ...     run_dir="/run_dir",
    ...     name="def")

`run_attr()` provides access to core run attributes. These are
considered publicly accessible attributes. In one case, `dir`, which
maps to `run_dir`, the attribute is renamed.

    >>> run_attr(run, "id")
    'abc'

    >>> run_attr(run, "name")
    'def'

    >>> run_attr(run, "dir")
    '/run_dir'

Create a run from a run meta directory.

    >>> meta_dir = make_temp_dir()

    >>> run = Run(
    ...     id="abc",
    ...     opref=OpRef("test", "test"),
    ...     meta_dir=meta_dir,
    ...     run_dir="/run_dir",
    ...     name="def")

    >>> init_run_meta(
    ...     run=run,
    ...     opdef=OpDef("test", {}),
    ...     config={},
    ...     cmd=OpCmd([], {})
    ... )

Various core attributes:

    >>> run_attr(run, "id")
    'abc'

    >>> run_attr(run, "name")
    'def'

    >>> run_attr(run, "dir")
    '/run_dir'

Started and stopped timestamps are read from the meta dir. Neither are
defined for `run` and attempting to read either generates an attribute
error.

    >>> run_attr(run, "started")
    Traceback (most recent call last):
    AttributeError: started

    >>> run_attr(run, "stopped")
    Traceback (most recent call last):
    AttributeError: stopped

Specify a default value for `run_attr` to return without raising an
error.

    >>> run_attr(run, "started", 123)
    123

    >>> run_attr(run, "stopped", 456)
    456

Write timestamps for started and stopped.

    >>> write(
    ...     path_join(run.meta_dir, "started"),
    ...     str(make_run_timestamp())
    ... )

    >>> write(
    ...     path_join(run.meta_dir, "stopped"),
    ...     str(make_run_timestamp())
    ... )

Re-read the attributes.

    >>> run_attr(run, "started")  # +wildcard
    datetime.datetime(...)

    >>> run_attr(run, "stopped")  # +wildcard
    datetime.datetime(...)

Reading an unsupported attribute generates an attribute error even if a
default is provided.

    >>> run_attr(run, "unknown")
    Traceback (most recent call last):
    AttributeError: unknown

    >>> run_attr(run, "unknown", 789)
    Traceback (most recent call last):
    AttributeError: unknown

When successfully read, attribute values are cached to avoid re-reading.

Modify the run ID.

    >>> run.id = "bca"

Re-reading does not change the run attribute value.

    >>> run_attr(run, "id")
    'abc'

To force a re-read, recreate the run.

    >>> run = Run(
    ...     id="cba",
    ...     opref=OpRef("test", "test"),
    ...     meta_dir=meta_dir,
    ...     run_dir="/run_dir",
    ...     name="def")

    >>> run_attr(run, "id")
    'cba'

In cases where recreating a run is not desireable, the cache can be
invalidated directly by accessing `run._cache`.

    >>> run._cache  # +pprint
    {'_attr_id': 'cba'}

Modify the run ID.

    >>> run.id = 'xxx'

The cached value is used.

    >>> run_attr(run, "id")
    'cba'

Invalidate the cache and read the label again.

    >>> run._cache.clear()

    >>> run_attr(run, "id")
    'xxx'

### User attributes

User attributes are written to a run user directory. This directory is
created along with initial attributes using `init_run_user_attrs()`.

Create a new run.

    >>> runs_dir = make_temp_dir()

    >>> run = make_run(OpRef("test", "test"), runs_dir)

Initialize user attributes.

    >>> init_run_user_attrs(run, {
    ...   "label": "Hello run",
    ...   "custom-123": 123
    ... })

`run_user_attrs()` reads a run's user attributes.

    >>> run_user_attrs(run)  # +pprint
    {'custom-123': 123, 'label': 'Hello run'}

### System attributes

### Logged attributes

TODO
