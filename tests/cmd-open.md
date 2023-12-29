# `open` command

`open` opens a run directory in the system file explorer. It can
optionally be used with a custom command.

    >>> run("gage open -h")  # +diff
    Usage: gage open [options] [run]
    ⤶
      Open a run in the file explorer.
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
                       system file explorer.
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

Default open location is the run directory. Use `echo` to print the
command args.

    >>> run("gage open --cmd echo")  # +parse
    {echo_out}
    <0>

    >>> assert echo_out == run_dir

Use `--path` to open a specific path in the run directory. We use the
`cat` command to print the file.

    >>> run("gage open --cmd cat --path summary.json")
    {
      "attributes": {
        "type": "example"
      },
      "metrics": {
        "speed": 0.1
      }
    }
    <0>

Hidden options are used for developer convenience.

`--summary` opens the meta summary file.

    >>> run("gage open --cmd cat --summary")
    {
      "attributes": {
        "type": "example"
      },
      "metrics": {
        "speed": 0.1
      }
    }
    <0>

Return value from command is passed through.

    >>> run("gage open --cmd false")
    <1>
