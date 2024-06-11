---
test-options: +skip=WINDOWS_FIX  # locking on Windows
---

# Starting a staged run

    >>> from gage._internal.run_util import *
    >>> from gage._internal.types import *

## Basic run

`exec_run()` executes a staged run. Once a run is staged, it's
independent of the originating project.

Create a sample project.

    >>> cd(make_temp_dir())

Use a simple Python script to print a message.

    >>> write("say.py", """
    ... msg = "Hi there"
    ... print(msg)
    ... """.lstrip())

Create an opdef that runs the script.

    >>> opdef = OpDef("test", {
    ...   "exec": {
    ...     "stage-sourcecode": "python -c \"print('Stage source')\"",
    ...     "run": "python say.py"
    ...   },
    ...   "config": "say.py"
    ... })

Create a new run.

    >>> runs_dir = make_temp_dir()
    >>> opref = OpRef("test", "test")
    >>> run = make_run(opref, runs_dir)

Initialize run meta.

    >>> config = {}
    >>> cmd = OpCmd(["python", "say.py"], {})
    >>> init_run_meta(run, opdef, {}, cmd)

Stage the run.

    >>> stage_run(run, ".")

    >>> ls(run.run_dir)
    say.py

Execute the run. `exec_run` blocks until the run process exits.

    >>> exec_run(run)

Run output is written to `output/40_run`.

List meta dir contents.

    >>> ls(run.meta_dir, permissions=True)  # +diff
    -r--r--r-- __schema__
    -r--r--r-- config.json
    -r--r--r-- id
    -r--r--r-- initialized
    -rw-rw-rw- log/files
    -rw-rw-rw- log/runner
    -rw-rw-rw- manifest
    -r--r--r-- opdef.json
    -r--r--r-- opref
    -r--r--r-- output/10_sourcecode
    -r--r--r-- output/10_sourcecode.index
    -r--r--r-- output/40_run
    -r--r--r-- output/40_run.index
    -r--r--r-- proc/cmd.json
    -r--r--r-- proc/env.json
    -r--r--r-- staged
    -r--r--r-- started

Note that some files are writeable. These are log, output, and manifest
files that are updated during the run and when the run is finalized.

Show run output.

    >>> cat(run_meta_path(run, "output", "10_sourcecode"))
    Stage source

    >>> cat(run_meta_path(run, "output", "40_run"))
    Hi there

At this point the run process has completed but the run is not yet
finalized. `finalize_run()` is responsible for finalizing a run.

Finalize the run.

    >>> finalize_run(run)

Show the meta files. All files are read only.

    >>> ls(run.meta_dir, permissions=True)  # +diff
    -r--r--r-- __schema__
    -r--r--r-- config.json
    -r--r--r-- id
    -r--r--r-- initialized
    -r--r--r-- log/files
    -r--r--r-- log/runner
    -r--r--r-- manifest
    -r--r--r-- opdef.json
    -r--r--r-- opref
    -r--r--r-- output/10_sourcecode
    -r--r--r-- output/10_sourcecode.index
    -r--r--r-- output/40_run
    -r--r--r-- output/40_run.index
    -r--r--r-- proc/cmd.json
    -r--r--r-- proc/env.json
    -r--r--r-- proc/exit
    -r--r--r-- staged
    -r--r--r-- started
    -r--r--r-- stopped
    -r--r--r-- summary.json

Show the run files.

    >>> ls(run.run_dir, permissions=True)  # +diff
    -r--r--r-- say.py

Show the finalize run manifest.

    >>> cat(run_meta_path(run, "manifest"))  # +diff
    s 3f9c639e1d6b056c071b44752ea97c694127291443065afa2689bf78ef3b8fb0 say.py

## Run with config

Start another run with different config.

    >>> run = make_run(opref, runs_dir)

    >>> config = {"msg": "Ho there"}
    >>> init_run_meta(run, opdef, config, cmd)

    >>> stage_run(run, ".")

Execute the run.

    >>> exec_run(run)

Show run output.

    >>> cat(run_meta_path(run, "output", "40_run"))
    Ho there
