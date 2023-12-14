# `gage.cli`

The `cli` library provides an interface for interacting with the user
over a command line interface.

    >>> from gage._internal import cli

    >>> cli.out("Hello")
    Hello

    >>> with StderrCapture() as err:
    ...     cli.err("Bye, sad")

    >>> err.print()
    Bye, sad
