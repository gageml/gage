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
      "columnDefs": [
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
          "field": "run:status"
        },
        {
          "field": "run:started"
        },
        {
          "field": "run:stopped"
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
          ...
          "metric:speed": 3,
          ...
        },
        {
          ...
          "metric:speed": 2,
          ...
        },
        {
          ...
          "metric:speed": 1,
          ...
        }
      ]
    }
    <0>
