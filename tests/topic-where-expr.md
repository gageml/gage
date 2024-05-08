# Where expression

The option `-w / --where` is used with various commands to filter
results.

Use the `hello` example to generate some runs.

    >>> use_example("hello")

    >>> run("gage run hello name=A --stage -y")  # +parse
    Run "{x:run_name}" is staged
    ⤶
    To start it, run 'gage run --start {y:run_name}'
    <0>

    >>> assert x == y

    >>> run("gage run hello name=B -y")
    Hello B
    <0>

    >>> run("gage run hello name=C -l 'a test' -y")
    Hello C
    <0>

    >>> run("gage runs -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | a test name=C                |
    | 2 | hello     | completed | name=B                       |
    | 3 | hello     | staged    | name=A                       |
    <0>

The current implementation of the where filter applies a general string
match algorithm to test runs. The test is applied to op name, status,
and label. TODO - include tags when implemented.

Show staged runs.

    >>> run("gage runs -s -w staged")
    | # | operation | status | description                     |
    |---|-----------|--------|---------------------------------|
    | 1 | hello     | staged | name=A                          |
    <0>

Show completed runs.

    >>> run("gage runs -s --where completed")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | a test name=C                |
    | 2 | hello     | completed | name=B                       |
    <0>

Show runs with 'test' (searches label).

    >>> run("gage runs -s --where test")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | a test name=C                |
    <0>

Filter on something that does not match.

    >>> run("gage runs -s -w does-not-match")
    | # | operation | status | description                     |
    |---|-----------|--------|---------------------------------|
    <0>

Empty string is equivalent to no filter.

    >>> run("gage runs -s --where ''")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | a test name=C                |
    | 2 | hello     | completed | name=B                       |
    | 3 | hello     | staged    | name=A                       |
    <0>
