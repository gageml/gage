# `gage` command

## Default command

Running `gage` without arguments shows help.

    >>> run("gage", cols=70)  # +diff
    Usage: gage [options] command
    ⤶
      Gage command line interface.
    ⤶
    Options:
      --version      Print program version and exit.
      -C path        Run command from a different directory.
      --runs path    Use a different location for runs.
      -v, --verbose  Show more information. Use twice for debug logging.
      -h, --help     Show this message and exit.
    ⤶
    Commands:
      archive          Archive runs.
      associate        Associate a run with a project directory.
      board            Show a board of run results.
      cat              Show a run file.
      check            Show and validate settings.
      comment          Comment on runs.
      copy             Copy runs.
      delete, del, rm  Delete runs.
      help             Show help for a topic.
      label            Set or clear run labels.
      list, ls, runs   List runs.
      open             Open a run or run file.
      operations, ops  Show available operations.
      publish          Publish a board.
      purge            Permanently delete runs.
      restore          Restore deleted or archived runs.
      run              Start or stage a run.
      select           Selects runs and their attributes.
      show             Show information about a run.
      util             Gage utility commands.
    <0>

## Help

Using `--help` shows help explicitly.

    >>> run("gage --help")  # +wildcard
    Usage: gage [options] command
    ⤶
      Gage command line interface.
    ⤶
    ...

## Version

    >>> run("gage --version")
    Gage 0.1.2
    <0>

## Changing cwd

The `-C` runs the command in the specified directory.

    >>> tmp = make_temp_dir()

    >>> run(f"gage -C {tmp} check -v", cols=999)  # +parse -space
    {}
    | command_directory   | {x:path} |
    | project_directory   | {}       |
    | gagefile            | {}       |
    | runs_directory      | {}       |
    <0>

    >>> assert x == tmp
