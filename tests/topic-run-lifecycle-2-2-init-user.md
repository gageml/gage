# Initializing run user attributes

Run user attributed are stored in a `.user` sidecar directory. This
directory remains always writeable to let users annotate runs after the
run is finalized.

    >>> from gage._internal.run_util import *
    >>> from gage._internal.types import *

Create a run.

    >>> runs_home = make_temp_dir()

    >>> run = make_run(OpRef("test", "test"), runs_home)

Initialize run user attributes.

    >>> init_run_user_attrs(run, {
    ...   "label": "colors",
    ...   "tags": ["red", "blue"]
    ... })

A `.user` directory is created for the run.

    >>> ls(runs_home)  # +parse
    {x:run_id}.meta/opref
    {y:run_id}.user/{:uuid4}.json

    >>> assert x == y == run.id
