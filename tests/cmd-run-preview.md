# `run` command - preview

Use `--preview` to show decisions made by the run command based on
configuration and the project state.

    >>> use_example("sourcecode")

    >>> run("gage run default --preview", cols=50)  # +table
    Source Code
    |                                                |
    | | patterns                              |      |
    | |---------------------------------------|      |
    | | **/* text size<100000 max-matches=500 |      |
    | | -**/.* dir                            |      |
    | | -**/* dir sentinel=bin/activate       |      |
    | | -**/* dir sentinel=.nocopy            |      |
    | | -summary.json                         |      |
    |                                                |
    |                                                |
    | | matched files  |                             |
    | |----------------|                             |
    | | .gitignore     |                             |
    | | data.txt       |                             |
    | | gage.toml      |                             |
    | | hello_world.py |                             |
    |                                                |
    <0>

    >>> run("gage run default --preview --json")  # +parse +diff
    {
      "sourcecode": {
        "src_dir": "{:path}",
        "patterns": [
          "**/* text size<100000 max-matches=500",
          "-**/.* dir",
          "-**/* dir sentinel=bin/activate",
          "-**/* dir sentinel=.nocopy",
          "-summary.json"
        ],
        "paths": [
          ".gitignore",
          "data.txt",
          "gage.toml",
          "hello_world.py"
        ]
      }
    }
    <0>

    >>> run("gage run pyfiles --preview --json")  # +parse
    {
      "sourcecode": {
        "src_dir": "{:path}",
        "patterns": [
          "*.py"
        ],
        "paths": [
          "hello_world.py"
        ]
      }
    }
    <0>

    >>> run("gage run exclude-data --preview --json")  # +parse +diff
    {
      "sourcecode": {
        "src_dir": "{:path}",
        "patterns": [
          "**/* text size<100000 max-matches=500",
          "-**/.* dir",
          "-**/* dir sentinel=bin/activate",
          "-**/* dir sentinel=.nocopy",
          "-summary.json",
          "-data.txt"
        ],
        "paths": [
          ".gitignore",
          "gage.toml",
          "hello_world.py"
        ]
      }
    }
    <0>

    >>> run("gage run all-files --preview --json")  # +parse
    {
      "sourcecode": {
        "src_dir": "{:path}",
        "patterns": [
          "**/*"
        ],
        "paths": [
          ".gitignore",
          "data.txt",
          "gage.toml",
          "hello_world.py",
          "hello_world.pyc"
        ]
      }
    }
    <0>
