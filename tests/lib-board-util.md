# Board Utils

    >>> from gage._internal.types import *

`board_util` provides support for loading boards and generating the
JSON-encodable data used to render boards.

    >>> from gage._internal.board_util import *

To show the default behavior, generate some runs.

    >>> use_project(sample("projects", "boards"))

    >>> run("gage run op-2 -y")
    <0>

    >>> run("gage run op-2 x=2 -y")
    <0>

    >>> run("gage ls -s")
    | # | operation | status    | label                        |
    |---|-----------|-----------|------------------------------|
    | 1 | op-2      | completed | x=2                          |
    | 2 | op-2      | completed | x=1                          |
    <0>

Use `board_data` to generate the board data for a list of runs.

    >>> from gage._internal.var import list_runs

    >>> runs = list_runs(sort=["-timestamp"])

By default, `board_data` includes columns for all core run attributes,
all config field, all attributes, and all metrics.

    >>> board_data(BoardDef({}), runs)  # +json +parse
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
