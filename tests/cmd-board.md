# `board` Command

The `board` command runs a basic board view (not implemented) or prints
board info as JSON (tested here).

    >>> use_example("summary")

Generate some runs with fake metrics.

    >>> run("gage run fake_speed=1 -qy")
    <0>
    >>> run("gage run fake_speed=2 -qy")
    <0>
    >>> run("gage run fake_speed=3 -qy")
    <0>

Show JSON data used by the board.

    >>> run("gage board --json")  # +wildcard +diff
    {
      "colDefs": [
        {
          "field": "run:status"
        },
        {
          "field": "run:id"
        },
        {
          "field": "run:name"
        },
        {
          "field": "run:operation"
        },
        {
          "field": "run:started"
        },
        {
          "field": "run:stopped"
        },
        {
          "field": "config:fake_speed"
        },
        {
          "field": "config:type"
        },
        {
          "field": "attribute:type"
        },
        {
          "field": "metric:speed"
        }
      ],
      "rowData": [
        {
          "__run__": {
            "id": "...",
            "name": "...",
            "operation": "default",
            "started": "...",
            "status": "completed",
            "stopped": "..."
          },
          "attribute:type": "example",
          "config:fake_speed": 3,
          "config:type": "example",
          "metric:speed": 3
        },
        {
          "__run__": {
            ...
          },
          "attribute:type": "example",
          "config:fake_speed": 2,
          "config:type": "example",
          "metric:speed": 2
        },
        {
          "__run__": {
            ...
          },
          "attribute:type": "example",
          "config:fake_speed": 1,
          "config:type": "example",
          "metric:speed": 1
        }
      ]
    }
    <0>

## Board run select

    >>> use_project(sample("projects", "boards"))

    >>> run("gage run foo=a bar=1 -y")
    <0>
    >>> run("gage run foo=a bar=2 -y")
    <0>
    >>> run("gage run foo=b bar=3 -y")
    <0>
    >>> run("gage run foo=b bar=4 -y")
    <0>

    >>> run("gage runs -s")
    | # | operation | status    | label                        |
    |---|-----------|-----------|------------------------------|
    | 1 | default   | completed | foo=b bar=4                  |
    | 2 | default   | completed | foo=b bar=3                  |
    | 3 | default   | completed | foo=a bar=2                  |
    | 4 | default   | completed | foo=a bar=1                  |
    <0>

The `group.yaml` board def selects the latest run grouped by `foo`.

    >>> run("gage board --json --config group.yaml")  # +wildcard
    {
      "colDefs": [
        {
          "field": "attribute:foo"
        },
        {
          "field": "attribute:bar"
        }
      ],
      "rowData": [
        {
          "__run__": ...
          "attribute:bar": 2,
          "attribute:foo": "a",
          "config:bar": 2,
          "config:foo": "a"
        },
        {
          "__run__": ...
          "attribute:bar": 4,
          "attribute:foo": "b",
          "config:bar": 4,
          "config:foo": "b"
        }
      ]
    }
    <0>

`group-first.yaml` selects the oldest runs within each group.

    >>> run("gage board --json --config group-first.yaml")  # +wildcard
    {
      "colDefs": [
        {
          "field": "attribute:foo"
        },
        {
          "field": "attribute:bar"
        }
      ],
      "rowData": [
        {
          "__run__": ...
          "attribute:bar": 1,
          "attribute:foo": "a",
          "config:bar": 1,
          "config:foo": "a"
        },
        {
          "__run__": ...
          "attribute:bar": 3,
          "attribute:foo": "b",
          "config:bar": 3,
          "config:foo": "b"
        }
      ]
    }
    <0>

Groups currently are only one level and must be specified as a field.
`group-missing-field.yaml` defines a group without specifying a field.

    >>> run("gage board --json --config group-missing-field.yaml")  # -space
    run-select group for board does not specify a field attribute: expected
    'attribute', 'metric', 'run-attr', or 'config'
    <1>

Gage currently limits group to selecting the most recent run. This will
be expanded to support generalized selection criteria. For now, groups
must specify 'latest' for 'started'.

    >>> run("gage board --json --config group-missing-started.yaml")  # -space
    run-select group must specify 'last' or 'first' for the 'started'
    attribute - this is a temporary limitation
    <1>

To Do - test:

- Col headerName when defined in metric summary
- Col headerName when defined in board def
- Other pass through attrs
- Use over overlapping fields (e.g. using both metric and attribute --
  should be invalid)
