# Where expression

The option `-w / --where` is used with various commands to filter
results.

Use the `hello` example to generate some runs.

    >>> use_example("hello")

    >>> run("gage run hello --stage -y")  # +parse
    Run "{x:run_name}" is staged
    ⤶
    To start it, run 'gage run --start {y:run_name}'
    <0>

    >>> assert x == y

    >>> run("gage run hello -y")
    Hello Gage
    <0>

    >>> run("gage runs -s")
    | # | operation | status    | label                        |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed |                              |
    | 2 | hello     | staged    |                              |
    <0>

Use a where expression to filter results.

    >>> run("gage runs -s -w staged")
    | # | operation | status | label                           |
    |---|-----------|--------|---------------------------------|
    | 1 | hello     | staged |                                 |
    <0>

    >>> run("gage runs -s --where completed")
    | # | operation | status    | label                        |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed |                              |
    <0>

Filter using an unsupported run status.

    >>> run("gage runs -s --where hah")
    Cannot use where expression 'hah': Unexpected character 'h' at line 1 col 1
    <1>
