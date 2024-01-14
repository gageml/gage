# `board` Command

The `board` command runs a basic board view (not implemented) or prints
board info as JSON (tested here).

    >>> use_example("summary")

Generate some runs with fake metrics.

    >>> run("gage run fake_speed=1 -qy")
    <0>
    >>> run("gage run fake_speed=2 -qy")
    <0>
    >>> run("gage run fake_speed=3 -l foo -qy")
    <0>

Show JSON data used by the board.

    >>> run("gage board --json")  # +wildcard +diff
    {
      "colDefs": [
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
          "field": "run:status"
        },
        {
          "field": "run:label"
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
          "attribute:type": "example",
          "config:fake_speed": 3,
          "config:type": "example",
          "metric:speed": 3,
          "run:id": "...",
          "run:label": "foo",
          "run:name": "...",
          "run:operation": "default",
          "run:started": "...",
          "run:status": "completed"
        },
        {
          "attribute:type": "example",
          "config:fake_speed": 2,
          "config:type": "example",
          "metric:speed": 2,
          "run:id": "...",
          "run:label": null,
          "run:name": "...",
          "run:operation": "default",
          "run:started": "...",
          "run:status": "completed"
        },
        {
          "attribute:type": "example",
          "config:fake_speed": 1,
          "config:type": "example",
          "metric:speed": 1,
          "run:id": "...",
          "run:label": null,
          "run:name": "...",
          "run:operation": "default",
          "run:started": "...",
          "run:status": "completed"
        }
      ]
    }
    <0>

Show CSV for the board.

    >>> run("gage board --csv")  # +wildcard
    run:id,run:name,run:operation,run:started,run:status,run:label,config:fake_speed,config:type,attribute:type,metric:speed
    ...,...,default,...,completed,foo,3,example,example,3
    ...,...,default,...,completed,,2,example,example,2
    ...,...,default,...,completed,,1,example,example,1
    <0>

## Group Select

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

    >>> run("gage board --json --config group.yaml")
    {
      "colDefs": [
        {
          "field": "attribute:foo",
          "headerName": "Foo"
        },
        {
          "field": "attribute:bar",
          "headerName": "Bar"
        }
      ],
      "rowData": [
        {
          "attribute:bar": 2,
          "attribute:foo": "a"
        },
        {
          "attribute:bar": 4,
          "attribute:foo": "b"
        }
      ]
    }
    <0>

    >>> run("gage board --csv --config group.yaml")
    Foo,Bar
    a,2
    b,4
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
          "attribute:bar": 1,
          "attribute:foo": "a"
        },
        {
          "attribute:bar": 3,
          "attribute:foo": "b"
        }
      ]
    }
    <0>

`group-max-bar.toml` groups by `foo` and selects runs with the max `bar`
value from each group.

    >>> run("gage board --json --config group-max-bar.toml")  # +wildcard
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
          "attribute:bar": 2,
          "attribute:foo": "a"
        },
        {
          "attribute:bar": 4,
          "attribute:foo": "b"
        }
      ]
    }
    <0>

`group-min-bar.toml` groups by `foo` and selects runs with the min `bar`
value from each group. Note that this example selects min `config`
rather than attribute.

    >>> run("gage board --json --config group-min-bar.toml")  # +wildcard
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
          "attribute:bar": 1,
          "attribute:foo": "a"
        },
        {
          "attribute:bar": 3,
          "attribute:foo": "b"
        }
      ]
    }
    <0>

### Group Select Errors

Group select requires a `group` field spec.

    >>> run("gage board --json --config group-missing-group.yaml")  # -space
    group-select for board is missing group-by field: expected run-attr,
    attribute, metric, or config
    <1>

Group must specify either `min` or `max` but not both.

    >>> run("gage board --json --config group-missing-min-max.yaml")  # -space
    group-select for board must specify either min or max fields
    <1>

`min` or `max` specs require valid field references.

    >>> run("gage board --json --config group-missing-min-field.yaml")  # -space
    group-select min for board is missing field: expected run-attr,
    attribute, metric, or config
    <1>

## Command validation

    >>> run("gage board")

    >>> run("gage board --csv --json")

To Do - test:

- Col headerName when defined in metric summary
- Col headerName when defined in board def
- Other pass through attrs
