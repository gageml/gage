# Run groups

UPDATE May 2024 - TODO: Review this test and delete if needed. I think
this is outdated but this might contain something worth keeping.

A run group is a run directory type used to group related runs.

Run groups use the same ID types as runs.

A run group directory uses the same structure as a run user directory.
Run groups log the following attributes:

- Member run IDs
- User attributes: label, tags, comments

Groups are shown in run lists when the `-g / --group` option is
specified.

Groups appear with the same columns as runs:

- ID / name: group ID (from `id` attr file in the group dir)

- Operation: derived from member runs - if all member runs share the
  same operation, that value is used, otherwise the operation is shown
  as `<run1.op>...<run2.op>`, where `<run1>` is the first member run in
  the group and `<run2>` is the second member run. Run started time is
  used to order runs in this case.

- Started: started time of the first member run

- Status: derived from member runs

  - If all runs are completed: `completed`
  - If any run is error: `error`
  - If any run is running: `running`
  - ???

- Label: `label` attribute from the group directory

Tags and comments are stored in the group directory as logged
attributes.

## Uses

Groups are used for a variety of cases.

- Group a run batch
- Group a pipeline
- Group runs generated on other systems (e.g. for distributed training)

A batch is generated using multi-value syntax with for flags.

For example, the following command runs a batch of three `train`
operations, each with a different learning rate.

    $ gage run train lr={1e-4,1e-3,1e-2}

In this case, Gage also generates a group.

## Example

    >>> from gage._internal.types import *

TODO: The content below is a prototype/scratch pad for this
functionality.

    >>> from gage._internal.attr_log import *

    >>> runs_dir = make_temp_dir()

    >> from gage._internal.run_group import *
    >> group = make_group(runs_dir)
    >> group.group_dir

Temp to simulate group:

    >>> from typing import NamedTuple
    >>> class RunGroup(NamedTuple):
    ...     id: str
    ...     group_dir: str
    >>> from gage._internal.run_util import make_run_id
    >>> group_id = make_run_id(1)
    >>> group = RunGroup(group_id, path_join(runs_dir, group_id + ".group"))

Create some runs for a group. Use explicit run IDs to define the group
later.

    >>> from gage._internal.run_util import make_run
    >>> run1 = make_run(OpRef("test", "test"), runs_dir, "aaa")
    >>> run2 = make_run(OpRef("test", "test"), runs_dir, "bbb")
    >>> run3 = make_run(OpRef("test", "test"), runs_dir, "ccc")

Create the group dir.

    >>> make_dir(group.group_dir)
    >>> write(path_join(group.group_dir, "id"), group.id)

    >>> ls(runs_dir)  # +parse
    {group_id:run_id}.group/id
    aaa.meta/opref
    bbb.meta/opref
    ccc.meta/opref

    >>> assert group_id == group.id

Write the group attributes.

    >>> log_attrs(group.group_dir, "test", {
    ...     "label": "Sample group",
    ...     "tag:color=green": "",
    ...     "run:aaa": "",
    ...     "run:bbb": "",
    ...     "run:ccc": ""
    ... })

    >>> get_attrs(group.group_dir)  # +parse +json
    {
      "label": "Sample group",
      "run:aaa": "",
      "run:bbb": "",
      "run:ccc": "",
      "tag:color=green": ""
    }
