# Phases example

The `phases` example creates files for each of the supported run phases:

- Stage source code
- Stage runtime
- Stage dependencies
- Run
- Finalize

    >>> use_example("phases")

    >>> run("gage ops")  # +table
    | operation | description                          |
    |-----------|--------------------------------------|
    | phases    | Run command for each supported phase |
    <0>

Run the operation.

    >>> run("gage run -y")
    <0>

Show the run files.

    >>> run("gage show --files")  # +table +parse
    | name                      | type              |     size |
    |---------------------------|-------------------|----------|
    | generated-file            | generated         |      0 B |
    | generated-file-2          | generated         |      0 B |
    | required-file             | dependency        |      0 B |
    | gage.toml                 | source code       |     {} B |
    | sourcecode-file           | source code       |      0 B |
    | touch.py                  | source code       |     {} B |
    | runtime-file              | runtime           |      0 B |
    <0>
