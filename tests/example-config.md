# Config example

The `config` example provides various config examples.

    >>> use_example("config")

## `hello`

`hello` uses a full key path in config to expose only the `name` value.

Source code for config:

Show the operation source code.

    >>> cat("hello.py")
    name = "Joe"
    times = 1
    ⤶
    for _ in range(times):
        print(f"Hello, {name}")

Run help:

    >>> run("gage run hello --help-op")  # -space
    Usage: gage run hello
    ⤶
     Sample hello op
    ⤶
       Flags
    |                         Default                          |
    |  name                   Joe                              |
    <0>

Run preview:

    >>> run("gage run hello", timeout=2)  # +parse
    You are about to run hello
    ⤶
     name  Joe
    ⤶
    Continue? (Y/n)
    <{:sigint}>

## Alternative config

Operations `hello-2` and `hello-3` provide the same configuration but
using different semantics.

    >>> run("gage run hello-2 -y")
    Hello, Joe
    <0>

    >>> run("gage run hello-3 -y")
    Hello, Joe
    <0>

`hello-4` uses the entire `hello.py` file and so includes support for
`times`.

    >>> run("gage run hello-4 name=Mike times=3 -y")
    Hello, Mike
    Hello, Mike
    Hello, Mike
    <0>
