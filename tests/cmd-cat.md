# `cat` command

Show command help.

    >>> run("gage cat --help")
    Usage: gage cat [options] [run]
    ⤶
      Show a run file.
    ⤶
    Arguments:
      [run]  Run to show file for. Value may be an index
             number, run ID, or run name. Default is latest
             run.
    ⤶
    Options:
      -p, --path path  Run file to show. Use 'gage show
                       --files' to show run files.
      -h, --help       Show this message and exit.
    <0>

Use `genfiles` sample project to generate a run with files we can show.

    >>> use_project(sample("projects", "genfiles"))

    >>> run("gage run gen-files -y")
    <0>

    >>> run("gage show --files", cols=37)  # +wildcard +table
    | name         | type        | size |
    |--------------|-------------|------|
    | msg.txt      | generated   |  ... |
    | gage.json    | source code |  ... |
    | gen_files.py | source code |  ... |
    <0>

Show run files using the `cat` command.

    >>> run("gage cat -p gage.json")
    {
        "gen-files": {
            "exec": "python gen_files.py"
        }
    }
    <0>

    >>> run("gage cat -p msg.txt")
    This
    is a message!
    <0>
