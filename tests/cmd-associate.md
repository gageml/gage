# `associate` command

    >>> run("gage associate -h")
    Usage: gage associate [options] run [project]
    ⤶
      Associate a run with a project directory.
    ⤶
      Local runs are associated with their projects by
      default. In cases where a run is imported from another
      system, use this command to link the run to a local
      project.
    ⤶
      To remove an associate, use the '--remove' option.
    ⤶
    Arguments:
      run        Run to associate. Value may be an index
                 number, run ID, or run name.
      [project]  Project directory to associate with run.
                 Required unless '--remove' is used.
    ⤶
    Options:
      -r, --remove  Remove any associate project with run.
      -h, --help    Show this message and exit.
    <0>

Local runs are associated with their respective project directories by
default.

Generate a run.

    >>> use_example("hello")

    >>> run("gage run -qy")
    <0>

    >>> run("gage select --project-dir")  # +parse
    {project_dir:path}
    <0>

    >>> compare_paths(project_dir, ".")
    True

    >>> ls(project_dir)
    gage.toml
    hello.py

Disassociate the project directory.

    >>> run("gage associate 1 --remove")  # +parse
    Removed project association for "{:run_id}"
    <0>

    >>> run("gage select --project-dir")
    <0>

Re-associate the project directory.

    >>> run("gage associate 1 .")  # +parse
    Associated "{:run_id}" with {project_dir:path}
    <0>

    >>> compare_paths(project_dir, ".")
    True

    >>> run("gage select --project-dir")  # +parse
    {x:path}
    <0>

    >>> assert x == project_dir
