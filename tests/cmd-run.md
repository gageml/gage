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
