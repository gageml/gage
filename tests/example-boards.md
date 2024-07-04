# 'boards' Example

    >>> use_example("boards")

    >>> run("gage ops")
    | operation           | description                        |
    |---------------------|------------------------------------|
    | op                  | A sample operation                 |
    <0>

Generate some runs.

    >>> run("gage run op x=0 -qy")
    <0>

    >>> run("gage run op x=1 -qy")
    <0>

    >>> run("gage run op x=2 -qy")
    <0>

    >>> run("gage runs -0")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | op        | completed | x=2 y=3 z=x                  |
    | 2 | op        | completed | x=1 y=2 z=x                  |
    | 3 | op        | completed | x=0 y=1 z=not x              |
    <0>

The default board shows columns x, y, and z.

    >>> run("gage board")
    | x | y | z     |
    |---|---|-------|
    | 2 | 3 | x     |
    | 1 | 2 | x     |
    | 0 | 1 | not x |
    <0>

`board-2.yaml` shows additional `score` columns. `score-1` is the
average of of x and y while `score-2` is a weighted average, giving
twice as much weight to y as to x.

    >>> run("gage board --config board-2.yaml")
    | x | y | z     | score-1 | score-2 |
    |---|---|-------|---------|---------|
    | 2 | 3 | x     | 2.5     | 2.667   |
    | 1 | 2 | x     | 1.5     | 1.667   |
    | 0 | 1 | not x | 0.5     | 0.6667  |
    <0>
