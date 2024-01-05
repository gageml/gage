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
      "description": null,
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
      ],
      "title": null
    }
    <0>

To Do - test:

- Col headerName when defined in metric summary
- Col headerName when defined in board def
- Other pass through attrs
- Use over overlapping fields (e.g. using both metric and attribute --
  should be invalid)
