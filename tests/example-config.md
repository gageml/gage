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

    >>> run("gage run hello --help-op")
    Usage: gage run hello
    ⤶
     Sample hello op
    ⤶
       Flags
    |                         Default                          |
    |  name                   Joe                              |
    <0>

Run preview:

    >>> run("gage run hello", timeout=1)
    You are about to run config:hello
    ⤶
     name  Joe
    ⤶
    Continue? (Y/n)
    <-9>

## Alternative config

Operations `hello-2` and `hello-3` provide the same configuration but
using different semantics.

    >>> run("gage run hello-2 -y")
    Hello, Joe
    <0>

    >>> run("gage run hello-3 -y")
    Hello, Joe
    <0>
