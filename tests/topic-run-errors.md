---
test-options: +skip=WINDOWS_FIX  # sample ops don't run on Windows
---
# Run Errors

These tests illustrate how Gage handles run errors. The use the
`run-errors` sample project.

    >>> use_project(sample("projects", "run-errors"))

    >>> run("gage ops", cols=54)
    | operation        | description                     |
    |------------------|---------------------------------|
    | deps-error       | Error in stage deps             |
    | exec-error       | Error in run exec               |
    | finalize-error   | Error in finalize               |
    | runtime-error    | Error in runtime init           |
    | sourcecode-error | Error in source code resolution |
    <0>

    >>> run("gage check .")  # +paths
    ./gage.toml is a valid Gage file
    <0>

Gage handles errors by exiting with the non-zero exit code received by
any run-related process. Processes include exec and any of the stage
phases.

    >>> run("gage run exec-error -y")
    <3>

    >>> run("gage run sourcecode-error -y")
    <4>

    >>> run("gage run runtime-error -y")
    <5>

    >>> run("gage run deps-error -y")
    <6>

    >>> run("gage run finalize-error -y")
    <7>
