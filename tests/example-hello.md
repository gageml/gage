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

    >>> run("gage ops")
    | operation | description             |
    |-----------|-------------------------|
    | hello     | Say hello to my friend. |
    <0>

Show help for `hello` op.

    >>> run("gage run hello --help-op")
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

    >>> run("gage show")  # +parse -space +diff
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
                           Configuration
    | name  Gage                                               |
    ⤶
                               Files
    | name            |type               |               size |
    | ----------------|-------------------|------------------- |
    | gage.toml       |source code        |              143 B |
    | hello.py        |source code        |               38 B |
    | ----------------|-------------------|------------------- |
    |                 |                   |       total: 181 B |
    ⤶
                               Output
    | Hello Gage                                               |
    <0>

List run files:

    >>> run_dir = path_join(runs_dir, run_id)

    >>> ls(run_dir, permissions=True)
    -r--r--r-- gage.toml
    -r--r--r-- hello.py

List meta files:

    >>> meta_dir = path_join(runs_dir, run_id + ".meta")

    >>> ls(meta_dir, permissions=True)
    -r--r--r-- __schema__
    -r--r--r-- config.json
    -r--r--r-- id
    -r--r--r-- initialized
    -r--r--r-- log/files
    -r--r--r-- log/runner
    -r--r--r-- manifest
    -r--r--r-- opdef.json
    -r--r--r-- opref
    -r--r--r-- output/40_run
    -r--r--r-- output/40_run.index
    -r--r--r-- proc/cmd.json
    -r--r--r-- proc/env.json
    -r--r--r-- proc/exit
    -r--r--r-- staged
    -r--r--r-- started
    -r--r--r-- stopped
    -r--r--r-- sys/platform.json

Show files log.

    >>> cat(path_join(meta_dir, "log", "files"))  # +parse
    a s {:timestamp} gage.toml
    a s {:timestamp} hello.py

Show runner log.

    >>> cat_log(path_join(meta_dir, "log", "runner"))  # +parse +diff -space
    Writing meta id
    Writing meta opdef
    Writing meta config
    Writing meta proc cmd
    Writing meta proc env
    Writing meta sys/platform
    Writing meta initialized
    Copying source code (see log/files):
      ['**/* text size<10000 max-matches=500',
       '-**/.* dir', '-**/* dir sentinel=bin/activate',
       '-**/* dir sentinel=.nocopy']
    Applying configuration (see log/patched)
    Finalizing staged files (see manifest)
    Writing meta staged
    Writing meta started
    Starting run process: python hello.py
    Writing meta proc/lock
    Writing meta stopped
    Writing meta proc/exit
    Deleting meta proc/lock
    Finalizing run files (see manifest)

## Custom name

Run with a different `name`.

    >>> run("gage run hello name=Joe -y")
    Hello Joe
    <0>

    >>> run("gage show")  # +parse
    {run_id:run_id}
    {}
    | exit_code  0                                             |
    ⤶
                           Configuration
    | name  Joe                                                |
    ⤶
                               Files
    | name            |type               |               size |
    | ----------------|-------------------|------------------- |
    | gage.toml       |source code        |              143 B |
    | hello.py        |source code        |               37 B |
    | ----------------|-------------------|------------------- |
    |                 |                   |       total: 180 B |
    ⤶
                               Output
    | Hello Joe                                                |
    <0>

Show files log.

    >>> meta_dir = path_join(runs_dir, run_id + ".meta")

    >>> cat(path_join(meta_dir, "log", "files"))  # +parse
    a s {:timestamp} gage.toml
    a s {:timestamp} hello.py

Show patched files.

    >>> cat(path_join(meta_dir, "log", "patched"))
    --- hello.py
    +++ hello.py
    @@ -1,3 +1,3 @@
    -name = "Gage"
    +name = "Joe"
    ⤶
     print(f"Hello {name}")

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

    >>> run("gage show")  # +parse -space
    {run_id:run_id}
    | hello:hello                                       staged |
    ⤶
                             Attributes
    | id         {x:run_id}                                    |
    | name       {short_name:rn}-{:rn}                         |
    {}
    <0>

    >>> assert x == run_id

    >>> run("gage ls -n2")  # +parse -space
    | #  | name    | operation       | started   | status      |
    |--{}--|
    | 1  | {x:rn}  | hello:hello     |           | staged      |
    | 2  | {:rn}   | hello:hello     | {}        | completed   |
    ⤶
     Showing 2 of 4 runs (use -m to show more)
    <0>

    >>> assert x == short_name

Show the runner log.

    >>> meta_dir = path_join(runs_dir, run_id + ".meta")

    >>> cat_log(path_join(meta_dir, "log", "runner"))  # +parse +diff -space
    Writing meta id
    Writing meta opdef
    Writing meta config
    Writing meta proc cmd
    Writing meta proc env
    Writing meta sys/platform
    Writing meta initialized
    Copying source code (see log/files):
      ['**/* text size<10000 max-matches=500',
       '-**/.* dir', '-**/* dir sentinel=bin/activate',
       '-**/* dir sentinel=.nocopy']
    Applying configuration (see log/patched)
    Finalizing staged files (see manifest)
    Writing meta staged

Start the staged run.

    >>> run(f"gage run --start {run_name} -y")
    Hello Robert
    <0>

    >>> run("gage ls -n2")  # +parse -space
    | #  | name    | operation       | started   | status      |
    |--{}--|
    | 1  | {x:rn}  | hello:hello     | {}        | completed   |
    | 2  | {:rn}   | hello:hello     | {}        | completed   |
    ⤶
     Showing 2 of 4 runs (use -m to show more)
    <0>

    >>> assert x == short_name

Show the runner log.

    >>> cat_log(path_join(meta_dir, "log", "runner"))  # +parse +diff -space
    Writing meta id
    Writing meta opdef
    Writing meta config
    Writing meta proc cmd
    Writing meta proc env
    Writing meta sys/platform
    Writing meta initialized
    Copying source code (see log/files):
      ['**/* text size<10000 max-matches=500',
       '-**/.* dir', '-**/* dir sentinel=bin/activate',
       '-**/* dir sentinel=.nocopy']
    Applying configuration (see log/patched)
    Finalizing staged files (see manifest)
    Writing meta staged
    Writing meta started
    Starting run process: python hello.py
    Writing meta proc/lock
    Writing meta stopped
    Writing meta proc/exit
    Deleting meta proc/lock
    Finalizing run files (see manifest)
