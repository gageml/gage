# Run Config

TODO: Lots to cover here topic wise. Initial motivation is to test
correct support of config defined in subdirs, given the dir-exclusion
optimization in `run_config`.

## Config file matching

Gage applies an optimization when searching for matching configuration
files under a project directory. This avoids expensive scans of
potentially large directories (e.g. `.gage/runs`, virtual env dirs,
local data dirs, etc.)

This test confirms that Gage correctly scans directories that may
contain potential config matches.

    >>> use_project(sample("projects", "config"))

    >>> run("gage run hello -y")
    Hi
    Hola
    <0>

    >>> run("gage show --files")
    | name         | type        | size |
    |--------------|-------------|------|
    | foo/hello.py | source code | 26 B |
    | hello.py     | source code | 24 B |
    <0>

## Config key shorthand

The `hello-2` operation includes two config keys, `msg1` and `msg2`. It
uses a shorthand version when defining `msg2` of `#msg2`, which applies
the previously specified path to the `hello2.py` file.

    >>> run("gage run hello-2 --help-op")  # +wildcard
    Usage: gage run hello-2
    ...
    |  msg1                   Hi                               |
    |  msg2                   Hola                             |
    <0>
