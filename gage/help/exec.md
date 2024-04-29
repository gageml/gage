# EXEC SPECIFICATION

An *exec* specification tells Gage what to run for an operation.

```toml
[train]

exec = "python train.py"
```

`exec` may be a string or a list of strings. A single string is run
using a system shell. A list of strings specifies the system command to
run, starting with the program name followed by program arguments.

The example above could also be specified as:

```toml
[train]

exec = ["python", "train.py"]
```

## SHELLS

When specified as a string, Gage runs the command using the default
system shell. On POSIX like systems (e.g. macOS, Linux, BSD) this is
`sh`. On Windows, the default shell is `cmd`.

You can specify a shell in the command using a "shebang". For example,
to use Nushell to run a command, `exec` may be specified as follows:

```toml
[train]

exec = """
#!bash
set -e
python train.py
python test.py
"""
```

## RUN LIFE CYCLE HOOKS

`exec` can be used to specify commands to run at various phases of a run
life cycle.

The following hooks are available:

| Hook                 | Use to                                  |
| -------------------- | --------------------------------------- |
| `stage-sourcecode`   | Add or modify source code files         |
| `stage-runtime`      | Initialize the runtime environment      |
| `stage-dependencies` | Add or modify run dependencies          |
| `run`                | Execute the run                         |
| `finalize-run`       | Perform steps after the run is executed |

Hooks are specified as `exec` attributes.

The following example creates a virtual environment and uses it for run.

``` toml
[train.exec]

init-runtime = """
virtualenv .venv
.venv/bin/pip install .
"""

run = "python -m train"

finalize-run = """
rm -rf .venv
"""
```
