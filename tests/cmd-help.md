# `help` command

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
