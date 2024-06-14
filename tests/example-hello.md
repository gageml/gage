---
test-options: +skip=WINDOWS_FIX - still issues with panels
---

# Hello example

The [`hello`](../examples/hello) example demonstrates the simplest
possible Gage project.

    >>> use_example("hello")

- Simple Gage file with one operation
- No language-specific features

    >>> cat("gage.toml")
    [hello]
    ⤶
    description = """
    Say hello to my friend.
    ⤶
    Sample operation that prints a greeting.
    """
    ⤶
    exec = "python hello.py"
    config = "hello.py"

List operations.

    >>> run("gage ops", cols=39)
    | operation | description             |
    |-----------|-------------------------|
    | hello     | Say hello to my friend. |
    <0>

Show help for `hello` op.

    >>> run("gage run hello --help-op")  # -space
    Usage: gage run hello
    ⤶
     Say hello to my friend.
    ⤶
     Sample operation that prints a greeting.
    ⤶
       Flags
    |                         Default                          |
    |  name                   Gage                             |
    <0>

## Default operation

Run hello.

    >>> run("gage run hello -y")
    Hello Gage
    <0>

Show the run. We skip on Windows due to issues with file size cols.
Windows is tested using a different show command below.

    >>> run("gage show")  # +parse +table +diff -windows
    {:run_id}
    | hello:hello                                    completed |
    ⤶
                             Attributes
    | id         {run_id:run_id}                               |
    | name       {:run_name}                                   |
    | started    {:datetime}                                   |
    | stopped    {:datetime}                                   |
    | location   {runs_dir:path}                               |
    | project    {:path}/examples/hello                        |
    | exit_code  0                                             |
    ⤶
                               Config
    | name  Gage                                               |
    ⤶
                               Files
    | name            |type               |               size |
    | ----------------|-------------------|------------------- |
    | gage.toml       |source code        |             {:d} B |
    | hello.py        |source code        |             {:d} B |
    | ----------------|-------------------|------------------- |
    | 2 files         |                   |      total: {:d} B |
    ⤶
                               Output
    | Hello Gage                                               |
    <0>

    >> run("gage show", env={"TERM": "DUMB"}, cols=80)
    ...   # +windows +panel +parse +diff
    +----------------------- {:run_id} -----------------------+
    | hello:hello                                   completed |
    +---------------------------------------------------------+
    +---------------------- Attributes -----------------------+
    | id         {run_id:run_id}                              |
    | name       {:run_name}                                  |
    | started    {:datetime}                                  |
    | stopped    {:datetime}                                  |
    | location   {runs_dir:path}                              |
    | project    {:path}\examples\hello                       |
    | exit_code  0                                            |
    +---------------------------------------------------------+
    +------------------------ Config -------------------------+
    | name  Gage                                              |
    +---------------------------------------------------------+
    +------------------------- Files -------------------------+
    | name           |type               |               size |
    | ---------------+-------------------+------------------- |
    | gage.toml      |source code        |              153 B |
    | hello.py       |source code        |               41 B |
    | ---------------+-------------------+------------------- |
    | 2 files        |                   |       total: 194 B |
    +---------------------------------------------------------+
    +------------------------ Output -------------------------+
    | Hello Gage                                              |
    +---------------------------------------------------------+
    <0>

List run files:

    >>> run_dir = path_join(runs_dir, run_id)

    >>> ls(run_dir)
    gage.toml
    hello.py

## Custom name

Run with different `name` config.

    >>> run("gage run hello name=Joe -y")
    Hello Joe
    <0>

    >>> run("gage show")  # +parse +table -windows
    {:run_id}
    {}
    | exit_code  0                                             |
    ⤶
                               Config
    | name  Joe                                                |
    ⤶
                               Files
    | name            |type               |               size |
    | ----------------|-------------------|------------------- |
    | gage.toml       |source code        |             {:d} B |
    | hello.py        |source code        |             {:d} B |
    | ----------------|-------------------|------------------- |
    | 2 files         |                   |      total: {:d} B |
    ⤶
                               Output
    | Hello Joe                                                |
    <0>

TODO: show patch info - mod show command??

## Flag with default operation

Run as default op with different name. Gage applies special handling to
prevent the flag assignment from being treated as the operation spec.

    >>> run("gage run name=Mike -y")
    Hello Mike
    <0>

## Stage a run

Stage `hello` with a custom name.

    >>> run("gage run hello name=Robert --stage -y")  # +parse
    Run "{run_name:run_name}" is staged
    ⤶
    To start it, run 'gage run --start {x:run_name}'
    <0>

    >>> assert x == run_name

Show the run.

    >>> run("gage show")  # +parse +panel
    {run_id:run_id}
    | hello:hello                                       staged |
    ⤶
                             Attributes
    | id         {x:run_id}                                    |
    | name       {short_name:rn}-{:rn}                         |
    {}
    <0>

    >>> assert x == run_id

    >>> run("gage ls -n2")  # +parse +table
    | #  | name    | operation       | started   | status      |
    |----|---------|-----------------|-----------|-------------|
    | 1  | {x:rn}  | hello           |           | staged      |
    | 2  | {:rn}   | hello           | {}        | completed   |
    ⤶
     Showing 2 of 4 runs (use -m to show more)
    <0>

    >>> assert x == short_name

Start the staged run.

    >>> run(f"gage run --start {run_name} -y")
    Hello Robert
    <0>

    >>> run("gage ls -n2")  # +parse +table
    | #  | name    | operation       | started   | status      |
    |----|---------|-----------------|-----------|-------------|
    | 1  | {x:rn}  | hello           | {}        | completed   |
    | 2  | {:rn}   | hello           | {}        | completed   |
    ⤶
     Showing 2 of 4 runs (use -m to show more)
    <0>

    >>> assert x == short_name
