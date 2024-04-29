# Help Topics

Gage provides separate help topics, which can be listed using `gage
help`.

    >>> run("gage help")  # +diff
    Usage: gage help [options] topic
    ⤶
      Show help for a topic.
    ⤶
    Options:
      -h, --help  Show this message and exit.
    ⤶
    Commands:
      exec        Defining run commands.
      filters     Filtering results.
      gagefile    Defining and using gage files.
      operations  Defining and running operations.
      remotes     Defining and using remote locations.
    <0>

Confirm that each topic shows something.

    >>> run("gage help exec")  # +wildcard
    # EXEC SPECIFICATION
    ⤶
    An *exec* specification tells Gage...

    >>> run("gage help filters")  # +wildcard
    # FILTERS
    ⤶
    Some commands show filterable results...

    >>> run("gage help gagefile")  # +wildcard
    # GAGE FILE
    ⤶
    ## OVERVIEW
    ⤶
    A Gage File specifies Gage ML operations...

    >>> run("gage help operations")  # +wildcard
    # OPERATIONS
    ⤶
    Operations specify how Gage ML runs something...

    >>> run("gage help remotes")  # +wildcard
    # REMOTES
    ⤶
    TODO - refer to rclone remote specs...
