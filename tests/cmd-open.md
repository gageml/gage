# `open` command

`open` opens a run directory in the system file explorer. It can
optionally be used with a custom command.

    >>> run("gage open -h")  # +diff
    Usage: gage open [options] [run]
    ⤶
      Open a run or run file.
    ⤶
      The default system program is used to open the specified
      path, which is the run directory by default. Use
      `--path` to specify a relative file path. Use `--cmd` to
      open the file with an alternative program.
    ⤶
    Arguments:
      [run]  Run to show information for. Value may be an
             index number, run ID, or run name. Default is
             latest run.
    ⤶
    Options:
      -p, --path path  Run file to open. Use 'gage show
                       --files' to show run files.
      -c, --cmd cmd    System command to use. Default is the
                       program associated with the path.
      -h, --help       Show this message and exit.
    <0>

Generate a run top open.

    >>> use_example("summary")
    >>> run("gage run default -y")
    Writing summary to summary.json
    <0>

    >>> run("gage select --run-dir")  # +parse
    {run_dir:path}
    <0>

Default open location is the run directory. Use `python` to print the command
args.

    >>> run("gage open --cmd \"python -c 'import sys; print(sys.argv[1:])'\"")  # +parse
    ['{x}']
    <0>

    >>> compare_paths(x, run_dir)
    True

Return value from command is passed through.

    >>> run("gage open --cmd \"python -c 'raise SystemExit(1)'\"")
    <1>
