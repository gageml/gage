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

## `write_summary`

The `write_summary` function writes a Gage summary file.

    >>> write_summary = gage.write_summary

By default, `write_summary` prints (echos) the summary fields to
standard output. This can be overridden by setting the `echo` parameter
to false.

By default, the function writes a file named `summary.json` to the
current directory on during a run. The filename can be changed using the
`filename` arg. To always write the summary file, even when a run is not
in progress, set `always_write` to true.

The function determines that a run is in progress when an environment
variable `RUN_DIR` corresponds to the current directory.

    >>> cd(make_temp_dir())

    >>> ls()
    <empty>

Confirm that `RUN_DIR` does not specify the current directory.

    >>> compare_paths(os.getenv("RUN_DIR") or "", os.getcwd())
    True

### Echoing Summary

Use `write_summary` to echo summary values By default, the function uses
a "flat" format to print fields.

The function doesn't print anything when summary is empty.

    >>> write_summary(metrics={}, attributes={})

Metrics printed in ascending order and are formatted using Python's 'g'
formatter when echos.

    >>> write_summary(metrics={"foo": 123, "bar": 456.789, "baz": -1.1122e4})
    bar: 456.789
    baz: -11122
    foo: 123

Confirm that the call did not write a file (see above for confirmation
that a run is not active).

    >>> ls()
    <empty>

Metrics and attributes are combined in sorted order.

    >>> write_summary(
    ...     metrics={"foo": 123},
    ...     attributes={"bar": "This is an attr"}
    ... )
    bar: This is an attr
    foo: 123

If a metric and attribute share the same name, each is printed with a
qualifier.

    >>> write_summary(
    ...     metrics={"foo": 123, "bar": 4.567},
    ...     attributes={"foo": "Hello"}
    ... )
    bar: 4.567
    foo (metric): 123
    foo (attribute): Hello

Summary can also be printed using JSON and YAML format.

    >>> summary = {
    ...     "attributes": {
    ...         "color": "red",
    ...         "type": "square"
    ...     },
    ...     "metrics": {
    ...         "height": 789,
    ...         "width": 987
    ...     }
    ... }

    >>> write_summary(**summary, echo_format="json")
    {
      "attributes": {
        "color": "red",
        "type": "square"
      },
      "metrics": {
        "height": 789,
        "width": 987
      }
    }

    >>> write_summary(**summary, echo_format="yaml")
    attributes:
      color: red
      type: square
    metrics:
      height: 789
      width: 987

Other formats (e.g. TOML) are not supported.

    >>> write_summary(**summary, echo_format="toml")
    Traceback (most recent call last):
    ValueError: toml

### Writing Summary File

Use `always_write` to write a summary file even when there is no active
run.

    >>> write_summary(**summary, always_write=True)
    color: red
    height: 789
    type: square
    width: 987

    >>> ls()
    summary.json

The summary file is always written as JSON, regardless of the echo
format.

    >>> cat("summary.json")
    {
      "attributes": {
        "color": "red",
        "type": "square"
      },
      "metrics": {
        "height": 789,
        "width": 987
      }
    }

Use `filename` to write a different file name. In this case we use
`echo=False` to disable printing to standard output.

    >>> write_summary(
    ...     metrics={"score": 0.998877},
    ...     echo=False,
    ...     always_write=True,
    ...     filename="alt.json"
    ... )

    >>> ls()
    alt.json
    summary.json

    >>> cat("alt.json")
    {
      "metrics": {
        "score": 0.998877
      }
    }

If a run is in progress, `write_summary` always writes the summary file.

Create a new current directory.

    >>> cd(make_temp_dir())

    >>> ls()
    <empty>

Specify the current directory as a `RUN_DIR` env var when calling
`write_summary`.

    >>> with Env({"RUN_DIR": os.getcwd()}):
    ...     write_summary(**summary)
    color: red
    height: 789
    type: square
    width: 987

    >>> ls()
    summary.json

    >>> cat("summary.json")
    {
      "attributes": {
        "color": "red",
        "type": "square"
      },
      "metrics": {
        "height": 789,
        "width": 987
      }
    }
