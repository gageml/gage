# Summary example

    >>> use_example("summary")

    >>> run("gage ops", cols=60)  # +diff
    | operation            | description                       |
    |----------------------|-----------------------------------|
    | alt-summary-metadata | Write summary vals with metadata  |
    | alt-summary-name     | Write alternative summary file    |
    |                      | name                              |
    | alt-summary-toml     | Write alternative summary file as |
    |                      | TOML                              |
    | default              | Write summary using default file  |
    |                      | name (default)                    |
    | output-summary       | Use output to print summary       |
    | output-summary-2     | Alt of output-summary using       |
    |                      | default pattern                   |
    | output-summary-3     | Alt of output summary using       |
    |                      | custom pattern                    |
    | output-summary-4     | Alt of output summary using       |
    |                      | invalid pattern                   |
    <0>

## Default Summary File

The default location for a run summary is `summary.json`.

    >>> run("gage run default -y")
    Writing summary to summary.json
    <0>

    >>> run("gage show --files", cols=60)  # +wildcard +table
    | name                | type        |   size |
    |---------------------|-------------|--------|
    | summary.json        | generated   |    ... |
    | gage.toml           | source code |    ... |
    | output_summary.py   | source code |    ... |
    | output_summary_2.py | source code |    ... |
    | output_summary_3.py | source code |    ... |
    | summary.py          | source code |    ... |
    | summary_metadata.py | source code |    ... |
    <0>

    >>> run("gage cat --path summary.json")
    {
      "attributes": {
        "type": "example"
      },
      "metrics": {
        "speed": 0.1
      }
    }
    <0>

    >>> run("gage show --summary")  # +table
    | name  | value   | type      |
    |-------|---------|-----------|
    | type  | example | attribute |
    | speed | 0.1     | metric    |
    <0>

## Alt Summary File

An op def may optionally specify the summary file name that it writes.
`alt-summary-name` tells Gage to find the summary JSON file
`results.json` instead of the default `summary.json`.

    >>> run("gage run alt-summary-name type=alt-example fake_speed=0.2 -y")
    Writing summary to results.json
    <0>

    >>> run("gage show --files")  # +wildcard +table
    | name                | type        |  size |
    |---------------------|-------------|-------|
    | results.json        | generated   |   ... |
    | gage.toml           | source code |   ... |
    | summary.py          | source code |   ... |
    | summary_metadata.py | source code |   ... |
    <0>

    >>> run("gage cat --path results.json")
    {
      "attributes": {
        "type": "alt-example"
      },
      "metrics": {
        "speed": 0.2
      }
    }
    <0>

    >>> run("gage show --summary")
    | name  | value       | type      |
    |-------|-------------|-----------|
    | type  | alt-example | attribute |
    | speed | 0.2         | metric    |
    <0>

## Alt Summary TOML

`alt-summary-toml` writes summary data to a TOML formatted file.

    >>> run("gage run alt-summary-toml type=alt-toml fake_speed=0.3 -y")
    Writing summary to zefile.toml
    <0>

    >>> run("gage show --files")  # +wildcard +table
    | name         | type        |  size |
    |--------------|-------------|-------|
    | zefile.toml  | generated   |   ... |
    | gage.toml    | source code |   ... |
    | summary.py   | source code |   ... |
    <0>

    >>> run("gage cat --path zefile.toml")
    [attributes]
    type = "alt-toml"
    ⤶
    [metrics]
    speed = 0.3
    <0>

    >>> run("gage show --summary")
    | name  | value    | type      |
    |-------|----------|-----------|
    | type  | alt-toml | attribute |
    | speed | 0.3      | metric    |
    <0>
