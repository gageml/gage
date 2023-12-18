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
