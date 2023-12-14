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

    >>> use_example("hello")
    >>> run("gage run -q -y")
    <0>

    >>> run("gage select")  # +parse
    {run_id:run_id}
    <0>

Use `echo` to print the command args.

    >>> run(f"gage open --cmd echo")  # +parse
    {echo_out}
    <0>

    >>> assert os.path.exists(echo_out)

    >>> assert os.path.basename(echo_out) == run_id
