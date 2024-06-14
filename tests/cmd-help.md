---
test-options: +skip=WINDOWS_FIX - still issues with panels
---

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
      batches     Define and run batches.
      exec        Define run commands.
      filters     Filter results.
      gagefile    Define and use gage files.
      operations  Define and run operations.
      remotes     Define and use remote locations.
    <0>

Note that topics are listed under the heading **Commands**, which is
misleading. This is due to our use of Typer/Click to implement help
topics as sub-commands. The Typer interface uses Rich formatting and
correctly uses **Topics** as the header. We see the style-free version
above.

When we unset `TERM`, the correct header is used.

    >>> run("gage help", env={"TERM": ""})  # +wildcard -space +skip=CI
    Usage: gage help [options] topic
    ⤶
      Show help for a topic.
    ⤶
    ... Topics ...
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
