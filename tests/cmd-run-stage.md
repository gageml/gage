# `run` command - stage run

    >>> use_example("hello")

    >>> from gage._internal import var

    >>> ls(var.runs_dir())
    <empty>

    >>> run("gage run hello --stage -y")  # +parse
    Run "{x:run_name}" is staged
    ⤶
    To start it, run 'gage run --start {y:run_name}'
    <0>

    >>> assert x == y

    >>> for path in lsl(var.runs_dir()):
    ...     print(path[36:])  # +diff
    .meta/__schema__
    .meta/config.json
    .meta/id
    .meta/initialized
    .meta/log/files
    .meta/log/runner
    .meta/manifest
    .meta/opdef.json
    .meta/opref
    .meta/proc/cmd.json
    .meta/proc/env.json
    .meta/staged
    .meta/sys/platform.json
    .project
    /gage.toml
    /hello.py

    >>> run("gage list")  # +parse
    | #   | name     | operation       | started    | status   |
    |-----|----------|-----------------|------------|----------|
    | 1   | {:rn}    | hello:hello     |            | staged   |
    <0>
