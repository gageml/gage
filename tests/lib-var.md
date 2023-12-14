# gage `var` support

The `var` module provides system wide data related services. It's primary
feature is to read Runs from a runs location.

    >>> from gage._internal import var

    >>> runs_home = make_temp_dir()

    >>> set_runs_home(runs_home)

    >>> var.list_runs()
    []
