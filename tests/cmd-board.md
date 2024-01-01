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
          "field": "run:id",
          "label": "Run ID"
        },
        {
          "field": "run:name",
          "label": "Run Name"
        },
        {
          "field": "run:operation",
          "label": "Operation"
        },
        {
          "field": "run:status",
          "label": "Run Status"
        },
        {
          "field": "run:started",
          "label": "Run Start"
        },
        {
          "field": "run:stopped",
          "label": "Run Stop"
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
          "__rowid__": "...",
          "attribute:type": "example",
          "metric:speed": 3,
          ...
        },
        {
          "__rowid__": "...",
          "attribute:type": "example",
          "metric:speed": 2,
          ...
        },
        {
          "__rowid__": "...",
          "attribute:type": "example",
          "metric:speed": 1,
          ...
        }
      ]
    }
    <0>

To Do - test:

- Col label when defined in metric summary
- Col label when defined in board def
