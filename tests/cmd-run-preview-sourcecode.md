# `run` command - preview source code

Use `--preview-sourcecode` to show source code that is copied for an
operation.

    >>> use_example("sourcecode")

    >>> run("gage run default --preview-sourcecode", env={"COLUMNS": "50"})  #+diff
    Source Code
    |                                                |
    | | patterns                             |       |
    | |--------------------------------------|       |
    | | **/* text size<10000 max-matches=500 |       |
    | | -**/.* dir                           |       |
    | | -**/* dir sentinel=bin/activate      |       |
    | | -**/* dir sentinel=.nocopy           |       |
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

    >>> run("gage run default --preview-sourcecode --json")  # +parse
    {
      "sourcecode": {
        "src_dir": "{:path}",
        "patterns": [
          "**/* text size<10000 max-matches=500",
          "-**/.* dir",
          "-**/* dir sentinel=bin/activate",
          "-**/* dir sentinel=.nocopy"
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

    >>> run("gage run pyfiles --preview-sourcecode --json")  # +parse
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

    >>> run("gage run exclude-data --preview-sourcecode --json")  # +parse
    {
      "sourcecode": {
        "src_dir": "{:path}",
        "patterns": [
          "**/* text size<10000 max-matches=500",
          "-**/.* dir",
          "-**/* dir sentinel=bin/activate",
          "-**/* dir sentinel=.nocopy",
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

    >>> run("gage run all-files --preview-sourcecode --json")  # +parse
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
