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

    >>> ls(run.meta_dir)  # +diff
    __schema__
    config.json
    id
    initialized
    log/files
    log/runner
    manifest
    opdef.json
    opref
    output/10_sourcecode
    output/10_sourcecode.index
    output/40_run
    output/40_run.index
    proc/cmd.json
    proc/env.json
    staged
    started

Note that some files are writeable. These are log, output, and manifest
files that are updated during the run and when the run is finalized.

Show run output.

    >>> cat(path_join(run.meta_dir, "output", "10_sourcecode"))
    Stage source

    >>> cat(path_join(run.meta_dir, "output", "40_run"))
    Hi there

    >>> ls(run.meta_dir)  # +diff
    __schema__
    config.json
    id
    initialized
    log/files
    log/runner
    manifest
    opdef.json
    opref
    output/10_sourcecode
    output/10_sourcecode.index
    output/40_run
    output/40_run.index
    proc/cmd.json
    proc/env.json
    staged
    started

At this point the run process has completed but the run is not yet
finalized. `finalize_run()` is responsible for finalizing a run.

Finalize the run.

    >>> finalized_run = finalize_run(run)

Finalize generates a zip meta directory. The run meta directory no
longer exists.

    >>> os.path.exists(run.meta_dir)
    False

The finalized run is updated to use the zipped meta directory.

    >>> os.path.basename(finalized_run.meta_dir)  # +parse
    '{x:run_id}.meta.zip'

    >>> assert x == run.id == finalized_run.id

Use `run_meta` to list the zip contents.

    >>> from gage._internal import run_meta

    >>> for name in run_meta.ls(finalized_run.meta_dir):
    ...     print(name)  # +diff
    __schema__
    config.json
    id
    initialized
    log/
    log/files
    log/runner
    manifest
    opdef.json
    opref
    output/
    output/10_sourcecode
    output/10_sourcecode.index
    output/40_run
    output/40_run.index
    proc/
    proc/cmd.json
    proc/env.json
    proc/exit
    staged
    started
    stopped
    summary.json

Show the run finalized files.

    >>> ls(finalized_run.run_dir, permissions=True)  # +diff
    -r--r--r-- say.py

Show the finalize run manifest.

    >>> with run_meta.open_manifest(finalized_run) as f:
    ...     print(f.read(), end="")  # -windows
    s 3f9c639e1d6b056c071b44752ea97c694127291443065afa2689bf78ef3b8fb0 say.py

    >>> with run_meta.open_manifest(finalized_run) as f:
    ...     print(f.read(), end="")  # +windows
    s 8e701bd990eafeb11b51ab584ea083e6e097d8c7f9a691166143dcef7f7256d0 say.py

## Run with config

Start another run with different config.

    >>> run = make_run(opref, runs_dir)

    >>> config = {"msg": "Ho there"}
    >>> init_run_meta(run, opdef, config, cmd)

    >>> stage_run(run, ".")

Execute the run.

    >>> exec_run(run)

Show run output.

    >>> run_meta.read_output(run, "40_run")
    'Ho there\n'
