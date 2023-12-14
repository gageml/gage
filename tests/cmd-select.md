# `select` command

`select` selects a run and shows its run ID.

Generate a sample run.

    >>> use_example("hello")
    >>> run("gage run -q -y")
    <0>

Select the latest run.

    >>> run("gage select")  # +parse
    {run_id:run_id}
    <0>

Select the latest run name.

    >>> run("gage select --name")  # +parse
    {run_name:run_name}
    <0>

Runs can be selected using index numbers, run ID, and run names.

    >>> run("gage select 1")  # +parse
    {x:run_id}
    <0>

    >>> assert x == run_id

    >>> run(f"gage select {run_id[:3]}")  # +parse
    {x:run_id}
    <0>

    >>> assert x == run_id

    >>> run(f"gage select {run_name[:3]}")  # +parse
    {x:run_id}
    <0>

    >>> assert x == run_id
