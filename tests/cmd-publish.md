# `publish` command

    >>> run("gage publish --help")  # +diff
    Usage: gage publish [options] [run]...
    ⤶
      Publish a board.
    ⤶
    Arguments:
      [run]...  Runs to publish. run may be a run ID, name,
                list index or slice. Default is to publish all
                runs.
    ⤶
    Options:
      --board-id ID      Board ID to publish. Required if 'id'
                         attribute is not specified in board
                         config.
      -w, --where expr   Publish runs matching filter
                         expression.
      -c, --config PATH  Use board configuration.
      --skip-runs        Don't copy runs.
      -y, --yes          Publish without prompting.
      -h, --help         Show this message and exit.
    <0>

Gage publishes a board and so requires a board ID. The ID must be
specified either as a command line option or as the `id` config
attribute.

Create a temp project.

    >>> use_project(make_temp_dir())

Run `publish` without specifying a board ID or config.

    >>> run("gage publish -y")  # -space
    gage: Missing board ID: use '--config <path>' to specify a
    board config or use '--board-id <id>'
    <1>

Config is specified using `--config`.

Create config without a board ID (`id` attribute).

    >>> write("board.json", "{}")

    >>> run("gage publish --config board.json -y")  # -space
    gage: Missing board ID: specify an 'id' attribute in board.json
    or use --board-id
    <1>

Publish requires an API token. This should be specified as the
environment variable `GAGE_TOKEN`.

Gage tokens start with 'gage_' - other formats generate an error.

    >>> run("gage publish --board-id test -y", env={"GAGE_TOKEN": "test"})
    gage: Invalid API token
    <1>

If `GAGE_TOKEN` is not specified, Gage prompts for a token if the
terminal is interactive. For tests, where the terminal is not
interactive, Gage does not prompt for a token.

    >>> run("gage publish --board-id test -y", timeout=2)
    gage: Missing API token: specify GAGE_TOKEN environment variable
    <1>

TODO:

- Interim generated files (board data)
- Copy runs

To support tests, `publish` should support publish to a local file
system (tmp).
