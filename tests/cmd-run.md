# `run` command

TODO - lots to check here!!

## Errors

Try to run an operation with no available Gage file.

    >>> cd(make_temp_dir())

Confirm that there is no available Gage file.

    >>> ls()
    <empty>

    >>> run("gage ops", env={"PROJECT_DIR": "."})
    gage: No operations defined for the current directory
    <1>

Try to run an operation.

    >>> run("gage run -y", env={"PROJECT_DIR": "."})
    gage: No operations defined for the current directory
    <1>

### Incompatible Options

Batch and start:

    >>> run("gage run -y --start barfoo --batch foobar")
    start and batch cannot be used together.
    ⤶
    Try 'gage run -h' for help.
    <1>

    >>> run("gage run -y --batch foobar --start barfoo")
    batch and start cannot be used together.
    ⤶
    Try 'gage run -h' for help.
    <1>

Start and stage:

    >>> run("gage run -y --start barfoo --stage")
    start and stage cannot be used together.
    ⤶
    Try 'gage run -h' for help.
    <1>

    >>> run("gage run -y --stage --start barfoo")
    stage and start cannot be used together.
    ⤶
    Try 'gage run -h' for help.
    <1>

Start and needed:

    >>> run("gage run -y --start foobar --needed")
    start and needed cannot be used together.
    ⤶
    Try 'gage run -h' for help.
    <1>
