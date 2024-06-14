# Run filter support

Run filters are implemented in `gage._internal.run_filter`.

    >>> from gage._internal.run_filter import *

Use `hello` sample project to generate runs to filter.

    >>> use_project(sample("projects", "filter"))

    >>> run("gage run no-op -l 1 -y")
    <0>
    >>> run("gage run no-op -l '2 DEV' -y")
    <0>
    >>> run("gage run hello -y")
    Hi
    <0>

Get the runs to filter.

    >>> from gage._internal import var

    >>> runs = var.list_runs(sort=["timestamp"])

Create a function to return run indexes for a given filter.

    >>> def apply_filter(filter):
    ...     return [i for i, run in enumerate(runs) if filter(run)]

## String match filters

`string_match_filter` implements a general string matching algorithm. It
returns runs whose attributes match a single string values.

The string is case-sensitive.

Attributes matched are:

- Operation name (full match)
- Status (full match)
- Label (partial match)
- Tags (full match of one tag) (TODO - tags not implemented)

The filter string cannot be empty.

    >>> apply_filter(string_match_filter(""))
    Traceback (most recent call last):
    ValueError: filter string cannot be empty

Operation name:

    >>> apply_filter(string_match_filter("no-op"))
    [0, 1]

    >>> apply_filter(string_match_filter("hello"))
    [2]

Run status:

    >>> apply_filter(string_match_filter("completed"))
    [0, 1, 2]

    >>> apply_filter(string_match_filter("terminated"))
    []

Run label:

    >>> apply_filter(string_match_filter("1"))
    [0]

    >>> apply_filter(string_match_filter("2"))
    [1]

    >>> apply_filter(string_match_filter("DEV"))
    [1]

No matches:

    >>> apply_filter(string_match_filter("x"))
    []

    >>> apply_filter(string_match_filter("not a match"))
    []
