---
parse-types:
  date: "\\d{4}-\\d{2}-\\d{2} \\d{2}:\\d{2}"
---

# `board` Command

The `board` command runs a basic board view (not implemented) or prints
board info as JSON (tested here).

    >>> run("gage board --help")  # +diff
    Usage: gage board [options] [run]...
    ⤶
      Show a board of run results.
    ⤶
    Arguments:
      [run]...  Runs to show. run may be a run ID, name, list
                index or slice. Default is to show all runs.
    ⤶
    Options:
      -w, --where expr   Show runs matching filter expression.
      -c, --config PATH  Use board configuration. Defaults to
                         board.json, board.yaml, or board.yaml
                         if present.
      --no-config        Don't use config.
      --csv              Show board as CSV output.
      --json             Show board as JSON output.
      -h, --help         Show this message and exit.
    <0>

Use `summary` example to demonstrate board command features.

    >>> use_example("summary")

Generate some runs with fake metrics.

    >>> run("gage run fake_speed=1 -qy")
    <0>
    >>> run("gage run fake_speed=2 -qy")
    <0>
    >>> run("gage run fake_speed=3 -l foo -qy")
    <0>

Show the board as a table.

    >>> run("gage board", cols=999)  # +table +parse
    | name | operation | started | status | label | fake_speed | type | type | speed |
    |-------------|---------|---------|-----------|-----|---|---------|---------|---|
    | {:run_name} | default | {:date} | completed | foo | 3 | example | example | 3 |
    | {:run_name} | default | {:date} | completed |     | 2 | example | example | 2 |
    | {:run_name} | default | {:date} | completed |     | 1 | example | example | 1 |
    <0>

Show JSON data used by the board.

    >>> run("gage board --json")  # +wildcard +diff
    {
      "colDefs": [
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

    >>> run("gage board --csv")  # +parse
    run:name,run:operation,run:started,run:status,run:label,config:fake_speed,config:type,attribute:type,metric:speed
    {:run_name},default,{:isodate},completed,foo,3,example,example,3
    {:run_name},default,{:isodate},completed,,2,example,example,2
    {:run_name},default,{:isodate},completed,,1,example,example,1
    <0>

## Summary Metadata

Summary values can be written either directly or as a `value` attribute
of a mapping. Mappings may include additional metadata alongside the
value. The `alt-summary-metadata` operation illustrates this.

    >>> run("gage run alt-summary-metadata -y")
    <0>

    >>> run("gage cat -p summary.json")
    {
      "metrics": {
        "foo": {
          "color": "green",
          "value": 1.123
        }
      }
    }
    <0>

When rendering a board as CSV content, values are selected from
mappings.

    >>> run("gage board --csv 1")  # +parse
    run:name,run:operation,run:started,run:status,run:label,metric:foo
    {:run_name},alt-summary-metadata,{:isodate},completed,,1.123
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

    >>> run("gage runs -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | default   | completed | bar=4 foo=b                  |
    | 2 | default   | completed | bar=3 foo=b                  |
    | 3 | default   | completed | bar=2 foo=a                  |
    | 4 | default   | completed | bar=1 foo=a                  |
    <0>

The `group.yaml` board def selects the latest run grouped by `foo`.

    >>> run("gage board --json --config group.yaml")  # +parse
    {
      "colDefs": [
        {
          "field": "attribute:foo",
          "label": "Foo"
        },
        {
          "field": "attribute:bar",
          "label": "Bar"
        }
      ],
      "rowData": [
        {
          "attribute:bar": 2,
          "attribute:foo": "a",
          "run:id": "{:run_id}"
        },
        {
          "attribute:bar": 4,
          "attribute:foo": "b",
          "run:id": "{:run_id}"
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

    >>> run("gage board --json --config group-first.yaml")  # +parse
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
          "attribute:foo": "a",
          "run:id": "{:run_id}"
        },
        {
          "attribute:bar": 3,
          "attribute:foo": "b",
          "run:id": "{:run_id}"
        }
      ]
    }
    <0>

`group-max-bar.toml` groups by `foo` and selects runs with the max `bar`
value from each group.

    >>> run("gage board --json --config group-max-bar.toml")  # +parse
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
          "attribute:foo": "a",
          "run:id": "{:run_id}"
        },
        {
          "attribute:bar": 4,
          "attribute:foo": "b",
          "run:id": "{:run_id}"
        }
      ]
    }
    <0>

`group-min-bar.toml` groups by `foo` and selects runs with the min `bar`
value from each group. Note that this example selects min `config`
rather than attribute.

    >>> run("gage board --json --config group-min-bar.toml")  # +parse
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
          "attribute:foo": "a",
          "run:id": "{:run_id}"
        },
        {
          "attribute:bar": 3,
          "attribute:foo": "b",
          "run:id": "{:run_id}"
        }
      ]
    }
    <0>

### Group Select Errors

Group select requires a `group` field spec.

    >>> run("gage board --json --config group-missing-group.yaml")  # -space
    gage: invalid board config: group-select for board is missing
    group-by field: expected run-attr, attribute, metric, or config
    <1>

Group must specify either `min` or `max` but not both.

    >>> run("gage board --json --config group-missing-min-max.yaml")  # -space
    gage: invalid board config: group-select for board must specify
    either min or max fields
    <1>

`min` or `max` specs require valid field references.

    >>> run("gage board --json --config group-missing-min-field.yaml")  # -space
    gage: invalid board config: group-select selector (min/max) for
    board is missing field: expected run-attr, attribute, metric, or config
    <1>

## Default Board Config

If the current directory has a file named `board.json`, `board.toml`, or
`board.yaml`, that file is used for config by default.

    >>> cd(make_temp_dir())

We start without any files.

    >>> ls()
    <empty>

Run `board` with `--verbose` to show the selected config.

    >>> run("gage --verbose board", env={"GAGE_RUNS": "."})
    Using default board config
    Generating board data for 0 run(s)
    <0>

Create the three board config candidates.

    >>> write("board.json", "{}")
    >>> write("board.toml", "")
    >>> write("board.yaml", "")

Gage selects `board.json` first.

    >>> run("gage --verbose board", env={"GAGE_RUNS": "."})  # +paths
    Using config from ./board.json
    Generating board data for 0 run(s)
    <0>

Delete `board.json` - Gage uses `board.toml`.

    >>> rm("board.json")

    >>> run("gage --verbose board", env={"GAGE_RUNS": "."})  # +paths
    Using config from ./board.toml
    Generating board data for 0 run(s)
    <0>

Delete `board.toml` - Gage uses `board.yaml`.

    >>> rm("board.toml")

    >>> run("gage --verbose board", env={"GAGE_RUNS": "."})  # +paths
    gage: Unexpected board config in "./board.yaml" - expected a map
    <1>

Use `--no-config` to explicit use default config.

    >>> run("gage --verbose board --no-config", env={"GAGE_RUNS": "."})
    Using default board config
    Generating board data for 0 run(s)
    <0>

## Command Validation

    >>> run("gage board --csv --json")
    gage: You can't use both --json and --csv options.
    <1>

    >>> run("gage board --no-config --config xxx")
    gage: You can't use both --config and --no-config options.
    <1>
