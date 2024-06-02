# Gage API

The public facing API is implemented by `gage._internal.api`. Its
exported functions are available from `gage`.

    >>> import gage

Generate some runs.

    >>> use_example("hello")

    >>> run("gage run hello -l run-1 -y")
    Hello Gage
    <0>

    >>> run("gage run hello name=Jane -l run-2 -y")
    Hello Jane
    <0>

Use the API to get the runs.

    >>> for r in gage.runs():
    ...     print(
    ...         r["operation"],
    ...         r["status"],
    ...         r["label"],
    ...         r["config"],
    ...         r["attributes"],
    ...         r["metrics"]
    ... )
    hello completed run-2 {'name': 'Jane'} {} {}
    hello completed run-1 {'name': 'Gage'} {} {}

Generate a run using the `summary` example.

    >>> use_example("summary")

    >>> run("gage run -y")
    Writing summary to summary.json
    <0>

    >>> for r in gage.runs():
    ...     print(r["operation"], r["status"], r["label"])
    ...     pprint(r["config"])
    ...     pprint(r["attributes"])
    ...     pprint(r["metrics"])
    default completed None
    {'fake_speed': 0.1, 'type': 'example'}
    {'type': 'example'}
    {'speed': 0.1}
