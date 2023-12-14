# `check` command

Help for `check`:

    >>> run("gage check --help", env={"COLUMNS": "72"})  # +diff
    Usage: gage check [options] [path]
    ⤶
      Show and validate settings.
    ⤶
      Shows Gage ML version, install location, and other configured
      settings.
    ⤶
      To check a Gage file for issues, specify path.
    ⤶
    Arguments:
      [path]  Gage file or a project directory to check. Cannot use with
              --version.
    ⤶
    Options:
      --version spec  Test Gage version against spec. Cannot be used with
                      path.
      --json          Format check output as JSON.
      -v, --verbose   Show more information.
      -h, --help      Show this message and exit.
    <0>

Default output:

    >>> run("gage check", cols=999)  # +parse -space
    | gage_version          | 0.1.0{}   |
    | gage_install_location | {:path}   |
    | python_version        | {:ver} {} |
    | python_exe            | {:path}   |
    | platform              | {}        |
    <0>

Verbose output shows default output plus additional settings. As some
settings are based on the current directory use `-C` with a temp
directory for control.

    >>> tmp = make_temp_dir()

    >>> run(f"gage -C {tmp} check -v")  # -space +parse
    | gage_version          | {}       |
    | gage_install_location | {}       |
    | python_version        | {}       |
    | python_exe            | {}       |
    | platform              | {}       |
    | command_directory     | {x:path} |
    | project_directory     | <none>   |
    | gagefile              | <none>   |
    <0>

    >>> assert x == tmp

## Check Gage file

Validate `hello` example.

    >>> use_example("hello")

    >>> run("gage check gage.toml")
    gage.toml is a valid Gage file
    <0>

If a directory is specified, `check` looks for a Gage file.

    >>> run("gage check .")
    ./gage.toml is a valid Gage file
    <0>

If a specified path doesn't exist, `check` exits with an error.

    >>> run("gage check not-there.toml")
    File "not-there.toml" does not exist
    <1>

Generate an invalid Gage file.

    >>> cd(make_temp_dir())

    >>> write("gage.json", """
    ... 123
    ... """)

    >>> run("gage check gage.json")
    There are errors in gage.json
    The instance must be of type "object"
    <1>

## Check version

The `--version` option is used to check the Gage version against a
version spec.

Matching spec:

    >>> from gage import __version__

    >>> run(f"gage check --version {__version__}")
    <0>

    >>> run(f"gage check --version =={__version__}")
    <0>

    >>> run(f"gage check --version '<={__version__}'")
    <0>

    >>> run(f"gage check --version '>={__version__}'")
    <0>

Non matching spec:

    >>> run(f"gage check --version '<{__version__}'")  # +parse
    Version mismatch: current version '{:ver}' does not match '<{:ver}'
    <1>

    >>> run(f"gage check --version '>{__version__}'")  # +parse
    Version mismatch: current version '{:ver}' does not match '>{:ver}'
    <1>

    >>> run("gage check --version 999")  # +parse
    Version mismatch: current version '{:ver}' does not match '999'
    <1>

    >>> run("gage check --version foobar")
    Invalid version spec 'foobar': missing comparison operator (==, <, >, etc.)
    See https://bit.ly/45AerAj for help with version specs.
    <1>

    >>> run("gage check --version ==foobar")  # -space
    Invalid version spec '==foobar': expected end or semicolon (after name and
    no valid version specifier)
    See https://bit.ly/45AerAj for help with version specs.
    <1>

## Incompatible params

    >>> run("gage check --version xxx some-path")
    path and version cannot be used together.
    ⤶
    Try 'gage check -h' for help.
    <1>
