# `run` command - operation help

    >>> use_example("hello")

    >>> run("gage run --help-op")  # +diff
    Usage: gage run
    ⤶
     Say hello to my friend.
    ⤶
     Sample operation that prints a greeting.
    ⤶
       Flags
    |                         Default                          |
    |  name                   Gage                             |
    <0>

If an operation doesn't define config, Gage omits the Flags section in
help.

    >>> use_project(make_temp_dir())

    >>> write("gage.json", """
    ... {"test": {}}
    ... """)

    >>> run("gage run test --help-op")
    Usage: gage run test
    <0>
