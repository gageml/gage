# `label` command

The `label` command sets and clears labels for runs.

Create two test runs.

    >>> use_example("hello")

    >>> run("gage run hello name=Joe -q -y")
    <0>

    >>> run("gage run hello name=Mike -q -y -l 'Saying hi'")
    <0>

Show the runs.

    >>> run("gage ls -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | Saying hi name=Mike          |
    | 2 | hello     | completed | name=Joe                     |
    <0>

Set the label for run 2.

    >>> run("gage label --set 'Say hi 2' 2 -y")
    Set label for 1 run
    <0>

    >>> run("gage ls -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | Saying hi name=Mike          |
    | 2 | hello     | completed | Say hi 2 name=Joe            |
    <0>

Clear the label for run 1.

    >>> run("gage label --clear 1 -y")
    Cleared label for 1 run
    <0>

    >>> run("gage ls -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | name=Mike                    |
    | 2 | hello     | completed | Say hi 2 name=Joe            |
    <0>

Set label for runs 1 and 2.

    >>> run("gage label --set 'Say hi to all' 1 2 -y")
    Set label for 2 runs
    <0>

    >>> run("gage ls -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | Say hi to all name=Mike      |
    | 2 | hello     | completed | Say hi to all name=Joe       |
    <0>

Clear all labels.

    >>> run("gage label --clear --all -y")
    Cleared label for 2 runs
    <0>

    >>> run("gage ls -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | name=Mike                    |
    | 2 | hello     | completed | name=Joe                     |
    <0>

## Errors

    >>> run("gage label -y")
    gage: Specify either '--set' or '--clear'
    ⤶
    Try 'gage label --help' for help.
    <1>

    >>> run("gage label 1 -y")
    gage: Specify either '--set' or '--clear'
    ⤶
    Try 'gage label --help' for help.
    <1>

    >>> run("gage label --set Hi -y")
    gage: Specify a run to modify or use '--all'.
    ⤶
    Use 'gage list' to show available runs.
    ⤶
    Try 'gage label -h' for additional help.
    <1>

    >>> run("gage label --clear -y")
    gage: Specify a run to modify or use '--all'.
    ⤶
    Use 'gage list' to show available runs.
    ⤶
    Try 'gage label -h' for additional help.
    <1>

    >>> run("gage label --set Hi --clear 1 -y")
    set and clear cannot be used together.
    ⤶
    Try 'gage label -h' for help.
    <1>
