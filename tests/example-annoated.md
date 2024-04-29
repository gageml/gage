# `annotated` example

    >>> use_example("annotated")

    >>> run("gage ops")
    | operation | description                                  |
    |-----------|----------------------------------------------|
    | add       | Adds x and y                                 |
    | hello     | Simplest possible Gage operation             |
    | hello-2   | Same as hello but with a configurable name   |
    | hello-3   | Same as hello but limits what source code is |
    |           | copied                                       |
    | incr      | Increments x by 1                            |
    <0>

    >>> run("gage run hello -y")
    Hello Joe
    <0>

    >>> run("gage show --files")  # +parse -space +paths
    | name                | type        |   size |
    {:|-|}
    | a-file.txt          | source code |     {} |
    | add.py              | source code |     {} |
    | data/big-file-1.txt | source code |     {} |
    | data/big-file-2.txt | source code |     {} |
    | gage.toml           | source code |     {} |
    | hello.py            | source code |     {} |
    <0>

    >>> run("gage run hello-2 name=Mike -y")
    Hello Mike
    <0>

    >>> run("gage run hello-3 -y")
    Hello Joe
    <0>

    >>> run("gage show --files")  # +parse -space
    | name                | type        |   size |
    {:|-|}
    | a-file.txt          | source code |     {} |
    | add.py              | source code |     {} |
    | hello.py            | source code |     {} |
    <0>

    >>> run("gage run add x=3 y=4 -y")
    3 + 4 = 7
    <0>

    >>> run("gage run incr x=3 -y")
    3 + 1 = 4
    <0>
