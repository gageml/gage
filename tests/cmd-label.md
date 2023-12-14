# `label` command

The `label` command sets and clears labels for runs.

Create two test runs.

    >>> use_example("hello")

    >>> run("gage run hello -q -y")
    <0>

    >>> run("gage run hello -q -y -l 'Say hi'")
    <0>

Show the runs.

    >>> run("gage ls -s")
    | # | operation   | status    | label                      |
    |---|-------------|-----------|----------------------------|
    | 1 | hello:hello | completed | Say hi                     |
    | 2 | hello:hello | completed |                            |
    <0>

Set the label for run 2.

    >>> run("gage label --set 'Say hi 2' 2 -y")
    Set label for 1 run
    <0>

    >>> run("gage ls -s")
    | # | operation   | status    | label                      |
    |---|-------------|-----------|----------------------------|
    | 1 | hello:hello | completed | Say hi                     |
    | 2 | hello:hello | completed | Say hi 2                   |
    <0>

Clear the label for run 1.

    >>> run("gage label --clear 1 -y")
    Cleared label for 1 run
    <0>

    >>> run("gage ls -s")
    | # | operation   | status    | label                      |
    |---|-------------|-----------|----------------------------|
    | 1 | hello:hello | completed |                            |
    | 2 | hello:hello | completed | Say hi 2                   |
    <0>

Set label for runs 1 and 2.

    >>> run("gage label --set 'Say hi to all' 1 2 -y")
    Set label for 2 runs
    <0>

    >>> run("gage ls -s")
    | # | operation   | status    | label                      |
    |---|-------------|-----------|----------------------------|
    | 1 | hello:hello | completed | Say hi to all              |
    | 2 | hello:hello | completed | Say hi to all              |
    <0>

Clear all labels.

    >>> run("gage label --clear --all -y")
    Cleared label for 2 runs
    <0>

    >>> run("gage ls -s")
    | # | operation   | status    | label                      |
    |---|-------------|-----------|----------------------------|
    | 1 | hello:hello | completed |                            |
    | 2 | hello:hello | completed |                            |
    <0>

## Errors

    >>> run("gage label -y")
    Specify either '--set' or '--clear'
    ⤶
    Try 'gage label --help' for help.
    <1>

    >>> run("gage label 1 -y")
    Specify either '--set' or '--clear'
    ⤶
    Try 'gage label --help' for help.
    <1>

    >>> run("gage label --set Hi -y")
    Specify a run to modify or use '--all'.
    ⤶
    Use 'gage list' to show available runs.
    ⤶
    Try 'gage label -h' for additional help.
    <1>

    >>> run("gage label --clear -y")
    Specify a run to modify or use '--all'.
    ⤶
    Use 'gage list' to show available runs.
    ⤶
    Try 'gage label -h' for additional help.
    <1>

    >>> run("gage label --set Hi --clear 1 -y")
    clear and set cannot be used together.
    ⤶
    Try 'gage label -h' for help.
    <1>
