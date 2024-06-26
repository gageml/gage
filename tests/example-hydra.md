---
test-options: +paths
---

# Hydra example

    >>> use_example("hydra")

    >>> ls(ignore=[".venv/**"])  # +diff
    .gitignore
    conf/__init__.py
    conf/config.yaml
    conf/db/mysql.yaml
    conf/db/postgresql.yaml
    gage.toml
    my_app.py

    >>> run("gage check .")
    ./gage.toml is a valid Gage file
    <0>

    >>> run("gage ops", cols=57)
    | operation | description                               |
    |-----------|-------------------------------------------|
    | my_app    | Replicates the Hydra Get Started example. |
    <0>

    >>> run("gage run my_app --preview --json")  # +diff +parse
    {
      "sourcecode": {
        "src_dir": "{}/examples/hydra",
        "patterns": [
          "**/* text size<100000 max-matches=500",
          "-**/.* dir",
          "-**/* dir sentinel=bin/activate",
          "-**/* dir sentinel=.nocopy",
          "-summary.json"
        ],
        "paths": [
          ".gitignore",
          "conf/__init__.py",
          "conf/config.yaml",
          "conf/db/mysql.yaml",
          "conf/db/postgresql.yaml",
          "gage.toml",
          "my_app.py"
        ]
      }
    }
    <0>
