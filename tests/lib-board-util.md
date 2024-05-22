# Board Utils

    >>> from gage._internal.types import *

`board_util` provides support for loading boards and generating the
JSON-encodable data used to render them.

    >>> from gage._internal.board_util import *

To show the default behavior, generate some runs.

    >>> use_project(sample("projects", "boards"))

    >>> run("gage run op-2 -y")
    <0>

    >>> run("gage run op-2 x=2 -y")
    <0>

    >>> run("gage ls -s", cols=64)
    | # | operation | status    | description                      |
    |---|-----------|-----------|----------------------------------|
    | 1 | op-2      | completed | x=2 speed=1.123 type=red x=2     |
    | 2 | op-2      | completed | x=1 speed=0.987 type=blue x=1    |
    <0>

Use `board_data` to generate the board data for a list of runs.

    >>> from gage._internal.var import list_runs

    >>> runs = list_runs(sort=["-timestamp"])
    >>> default_board = board_data(BoardDef({}), runs)

By default, `board_data` includes columns for default run attributes,
all config, all attributes, and all metrics.

    >>> default_board  # +json +parse
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
          "field": "config:x"
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
          "attribute:type": "red",
          "config:x": 2,
          "metric:speed": 1.123,
          "run:id": "{}",
          "run:label": "x=2",
          "run:name": "{}",
          "run:operation": "op-2",
          "run:started": "{}",
          "run:status": "completed"
        },
        {
          "attribute:type": "blue",
          "config:x": 1,
          "metric:speed": 0.987,
          "run:id": "{}",
          "run:label": "x=1",
          "run:name": "{}",
          "run:operation": "op-2",
          "run:started": "{}",
          "run:status": "completed"
        }
      ]
    }

Note that the default col defs include run name but do not include run ID.

    >>> any(col["field"] == "run:name" for col in default_board["colDefs"])
    True

    >>> any(col["field"] == "run:id" for col in default_board["colDefs"])
    False

This mirrors the data presented in `gage runs` lists, which uses name to
identify runs rather than ID.

    >>> run("gage runs")  # +parse +table
    | #  | name     | operation     | started    | status      |
    |----|----------|---------------|------------|-------------|
    | 1  | {}       | op-2          | {}         | completed   |
    | 2  | {}       | op-2          | {}         | completed   |
    <0>

The `run:id` field, however, always appears in row data.

    >>> all("run:id" in row for row in default_board["rowData"])
    True

This ensures that board data is always associated with the underlying
run.

## Filter Runs

Boards may define a `run-select` attribute, which can limit the runs
included in the board. `board-op-2.toml` selects only `op-2` runs. Use
`filter_board_runs` to apply the filter.

Use `load_board_def` to load the board in `board-op-2.toml`.

    >>> board = load_board_def(sample("projects", "boards", "board-op-2.toml"))

Generate another run. Use `default`.

    >>> run("gage run -y")
    <0>

    >>> runs = list_runs(sort=["-timestamp"])

    >>> [run.opref.op_name for run in runs]
    ['default', 'op-2', 'op-2']

Filter the runs using `filter_board_runs`.

    >>> [run.opref.op_name for run in filter_board_runs(runs, board)]
    ['op-2', 'op-2']

## Field Values

Operation may include additional attributes alongside summary values.
`op-3` illustrates this.

Reset the project.

    >>> use_project(sample("projects", "boards"))

Generate an `op-3` run.

    >>> run("gage run op-3 -y")
    <0>

The run summary includes values that have addition attributes.

    >>> run("gage cat -p summary.json")
    {
      "attributes": {
        "color": {
          "links": [
            "/color",
            "/attr"
          ],
          "value": "red"
        }
      },
      "metrics": {
        "height": 1.2,
        "width": {
          "comment": "2x height",
          "value": 2.4
        }
      }
    }
    <0>

Field attributes are included in the board data.

    >>> board_data(BoardDef({}), list_runs())  # +json +parse
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
          "field": "attribute:color"
        },
        {
          "field": "metric:height"
        },
        {
          "field": "metric:width"
        }
      ],
      "rowData": [
        {
          "attribute:color": {
            "links": [
              "/color",
              "/attr"
            ],
            "value": "red"
          },
          "metric:height": 1.2,
          "metric:width": {
            "comment": "2x height",
            "value": 2.4
          },
          "run:id": "{}",
          "run:label": null,
          "run:name": "{}",
          "run:operation": "op-3",
          "run:started": "{}",
          "run:status": "completed"
        }
      ]
    }

Note that the `metric:height` value is defined directly as the number
`1.2`, rather than be specified as `{"value": 1.2}`. These are
equivalent expressions. Gage uses the value directly when it can.

## Column Definitions

Board defs may define columns explicitly. These are used instead of the
default list.

    >>> board = load_board_def(sample("projects", "boards", "board-op-3.json"))

    >>> board_data(board, list_runs())  # +json +parse +diff
    {
      "colDefs": [
        {
          "field": "run:id"
        },
        {
          "field": "run:operation"
        },
        {
          "field": "run:op_ns"
        },
        {
          "field": "run:op_version",
          "label": "Operation Ver"
        },
        {
          "field": "attribute:color"
        },
        {
          "field": "metric:height"
        },
        {
          "field": "metric:width",
          "label": "The Width",
          "sort": "desc"
        }
      ],
      "rowData": [
        {
          "attribute:color": {
            "links": [
              "/color",
              "/attr"
            ],
            "value": "red"
          },
          "metric:height": 1.2,
          "metric:width": {
            "comment": "2x height",
            "value": 2.4
          },
          "run:id": "{:run_id}",
          "run:op_ns": "boards",
          "run:op_version": null,
          "run:operation": "op-3"
        }
      ]
    }

Note that column def attributes are also passed through to the board
data.

## Safe JSON Vals

Browser JSON does not accept `NaN` values. The function `_safe_json_val`
converts `NaN` to `null`. It applies the conversion to values in objects
and arrays.

    >>> from gage._internal.board_util import _safe_json_val

    >>> NaN = float("NaN")

    >>> _safe_json_val(NaN)  # +json
    null

    >>> _safe_json_val({"foo": NaN, "bar": 1})  # +json
    {
      "bar": 1,
      "foo": null
    }

    >>> _safe_json_val({"foo": [1, NaN]})  # +json
    {
      "foo": [
        1,
        null
      ]
    }

## Key Case Conversion

Gage board is configured using kebab case for keys. This is in keeping
conventions used in VS Code, which inspires Gage config in general.

The function `_apply_col_def_key_case` converts kebab case keys to camel
case.

    >>> from gage._internal.board_util import _apply_col_def_key_case

    >>> _apply_col_def_key_case({})
    {}

    >>> _apply_col_def_key_case({
    ...     "foo": 123,
    ...     "foo-bar": 456,
    ...     "baz": {"baz-bam": "super duper"}
    ... })  # +json
    {
      "baz": {
        "bazBam": "super duper"
      },
      "foo": 123,
      "fooBar": 456
    }
