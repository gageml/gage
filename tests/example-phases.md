---
test-options: +skip=WINDOWS_FIX  # relies on "touch" cmd, os-specific
---

# Phases example

The `phases` example creates files for each of the supported run phases:

- Stage source code
- Stage runtime
- Stage dependencies
- Run
- Finalize

    >>> use_example("phases")

    >>> run("gage ops")
    | operation | description                          |
    |-----------|--------------------------------------|
    | phases    | Run command for each supported phase |
    <0>

Run the operation.

    >>> run("gage run -y")
    <0>

Show the run files.

    >>> run("gage show --files")
    | name             | type        |  size |
    |------------------|-------------|-------|
    | generated-file   | generated   |   0 B |
    | generated-file-2 | generated   |   0 B |
    | required-file    | dependency  |   0 B |
    | gage.toml        | source code | 314 B |
    | sourcecode-file  | source code |   0 B |
    | runtime-file     | runtime     |   0 B |
    <0>
