# Staging a run dir

    >>> from gage._internal.run_util import *
    >>> from gage._internal.types import *

A staged run can be started by a runner using only the run directory and
the contents of the run meta directory. Staged runs (i.e. the run and
meta directories) can be relocated to another compatible system and
started there.

A staged run is independent of its project.

A staged run relies on a compatible system (platform, installed
applications and libraries, etc.) to run. If a staged run is moved to an
incompatible system, it won't run.

To stage a run, the runner must copy all files required by the run that
are not otherwise provided by the system to the run directory.

When staging a run, meta is updated with a manifest file and additional
log entries, which reflect the changes made to the run directory during
staging.

## Run meta

Run meta must be initialized before the run can be staged. For more
information on this process, see [*Initializing run
meta*](topic-run-lifecycle-2-init-meta.md).

Create a run and initialize its meta.

    >>> runs_home = make_temp_dir()
    >>> opref = OpRef("test", "test")
    >>> run = make_run(opref, runs_home)

Create the op def.

    >>> opdef = OpDef(
    ...     "test",
    ...     {
    ...         "sourcecode": None,  # Use defaults
    ...         "config": {"keys": "train.py#*"},
    ...         "exec": {
    ...             "stage-sourcecode": [
    ...                 sys.executable, "-c",
    ...                 "open('msg.txt', 'w').write('hello')"
    ...             ],
    ...             "stage-runtime": [
    ...                 sys.executable, "setup.py"
    ...             ]
    ...         }
    ...     })

The op def configures the operation as follows:

- Use default source code copy rules (e.g. plain text files)
- Apply configuration to variables in train.py
- Perform additional steps during source code staging (write `msg.txt`)
  and during runtime staging (run `setup.py`)

Create configuration, which is applied to the source code files
according to `config` definitions in op def.

    >>> config = {"x": 2}

Define the operation command and environment.

    >>> cmd = OpCmd(["python", "train.py"], {})

List meta files before init.

    >>> ls(run.meta_dir, include_dirs=True, permissions=True)
    -r--r--r-- opref

Initialize run meta.

    >>> init_run_meta(run, opdef, config, cmd, {})

List meta files after init.

    >>> ls(run.meta_dir, include_dirs=True, permissions=True)  # +diff
    -r--r--r-- __schema__
    -r--r--r-- config.json
    -r--r--r-- id
    -r--r--r-- initialized
    drwxrwxr-x log
    -rw-rw-r-- log/runner
    -r--r--r-- opdef.json
    -r--r--r-- opref
    drwxrwxr-x proc
    -r--r--r-- proc/cmd.json
    -r--r--r-- proc/env.json

## Run stage phases

The tests in this section show each of the staging phases:

- Copy source code
- Apply configuration
- Initialize runtime
- Resolve dependencies
- Finalize staged run

The order of these phases is important:

1. Source code must be copied before configuration is applied (source
   code < config)
2. A runtime may require configured source code (config < runtime)
3. Dependency resolution may required an initialized runtime (runtime <
   dependencies)
4. All files must be written before a staged run is finalized
   (everything < finalize)

Each phase is executed by `stage_run()` in the appropriate order.

Changes to the run directory are logged in `log/files` for each phase.
This record is used to generate the run manifest.

## Source code

`stage_sourcecode()` requires a source directory and a run. The rules for
source code copy are defined in the run op def or are applied as
defaults if rules are not provided.

Create a sample source code directory structure with a `train.py` script
that will be used in the application of configuration later.

    >>> sourcecode_dir = make_temp_dir()

Create a sample train script. This defines the config referenced in the
op def.

    >>> write(path_join(sourcecode_dir, "train.py"), """
    ... x = 1
    ... print(f"loss = {x - 1}")
    ... """.strip())

Create a sample runtime init script. This is executed in the stage
runtime hook in the op def.

    >>> write(path_join(sourcecode_dir, "setup.py"), """
    ... import os
    ... print("Simulating a virtual env install")
    ... os.makedirs(os.path.join(".venv", "bin"))
    ... open(os.path.join(".venv", "bin", "activate"), "w").close()
    ... """)

Create other sample source code files (empty).

    >>> touch(path_join(sourcecode_dir, "eval.py"))
    >>> touch(path_join(sourcecode_dir, "gage.toml"))
    >>> make_dir(path_join(sourcecode_dir, "conf"))
    >>> touch(path_join(sourcecode_dir, "conf", "train.yaml"))
    >>> touch(path_join(sourcecode_dir, "conf", "eval.yaml"))

    >>> ls(sourcecode_dir)  # +diff
    conf/eval.yaml
    conf/train.yaml
    eval.py
    gage.toml
    setup.py
    train.py

Default source code copy rules are applied as the op def doesn't
otherwise specify rules.

    >>> opdef = meta_opdef(run)

    >>> opdef.get_sourcecode()  # +pprint
    None

Confirm the run directory is empty.

    >>> ls(run.run_dir)
    <empty>

Copy the source code to the run directory.

    >>> stage_sourcecode(run, sourcecode_dir)

Source code files are copied and left in a writeable state.

    >>> ls(run.run_dir, permissions=True)  # +diff
    -rw-rw-r-- conf/eval.yaml
    -rw-rw-r-- conf/train.yaml
    -rw-rw-r-- eval.py
    -rw-rw-r-- gage.toml
    -rw-rw-r-- msg.txt
    -rw-rw-r-- setup.py
    -rw-rw-r-- train.py

The list of files is written to the files log. Log entries are per line
and consist of an event, a file type, a modified timestamp, and a path.

    >>> cat(run_meta_path(run, "log", "files"))  # +parse
    a s {:timestamp} conf/eval.yaml
    a s {:timestamp} conf/train.yaml
    a s {:timestamp} eval.py
    a s {:timestamp} gage.toml
    a s {:timestamp} msg.txt
    a s {:timestamp} setup.py
    a s {:timestamp} train.py

In this case, "a" means the file was added. "s" means it's a source code
type.

The runner log contains the applied include and exclude patterns.

    >>> cat_log(run_meta_path(run, "log", "runner"))  # +wildcard -space
    Writing meta id
    ...
    Copying source code (see log/files):
      ['**/* text size<10000 max-matches=500',
       '-**/.* dir',
       '-**/* dir sentinel=bin/activate',
       '-**/* dir sentinel=.nocopy']
    Running stage-sourcecode (see output/10_sourcecode):
      ['...',
       '-c',
       "open('msg.txt', 'w').write('hello')"]
    Exit code for stage-sourcecode: 0

`output/10_sourcecode` contains the output generated by the
stage-sourcecode hook. In this case it's empty.

    >>> cat(run_meta_path(run, "output", "10_sourcecode"))
    <empty>

## Apply config

`apply_config()` applies configuration defined in meta `config.json` to
run files. Config values are applied according to the config paths in
the opdef.

The config values to apply:

    >>> cat(run_meta_path(run, "config.json"))
    {
      "x": 2
    }

The op def config:

    >>> cat(run_meta_path(run, "opdef.json"))  # +wildcard
    {
      "config": {
        "keys": "train.py#*"
      },
    ...

The target for config is `train.py`. Prior to the application of config,
`x` is 1 in `train.py`.

    >>> cat(path_join(run.run_dir, "train.py"))
    x = 1
    print(f"loss = {x - 1}")

Apply run config.

    >>> apply_config(run)

Changes are logged in run meta under `log/patched`.

    >>> cat(run_meta_path(run, "log", "patched"))
    --- train.py
    +++ train.py
    @@ -1,2 +1,2 @@
    -x = 1
    +x = 2
     print(f"loss = {x - 1}")

`train.py` is modified.

    >>> cat(path_join(run.run_dir, "train.py"))
    x = 2
    print(f"loss = {x - 1}")

## Runtime

`stage_runtime()` calls the `stage-runtime` hook, if defined. This is
used to install and configure any runtime requirements in preparation
for the run.

The op def is configured to run `setup.py` to stage the runtime.

    >>> stage_runtime(run, sourcecode_dir)

The runner log records the command and its exit code.

    >>> cat_log(run_meta_path(run, "log", "runner"))  # +wildcard -space
    Writing meta id
    ...
    Running stage-runtime (see output/20_runtime):
      ['...', 'setup.py']
    Exit code for stage-runtime: 0

`output/20_runtime` contains the output from the hook script.

    >>> cat(run_meta_path(run, "output", "20_runtime"))
    Simulating a virtual env install

## Resolve dependencies

TODO: `resolve_dependencies()`

## Finalize staged run

As the last phase of staging, two changes are made:

- Run directory files are made read-only
- Run manifest is written
- Meta `staged` timestamp is written

Files copied to the run directory for staging are typically inputs only
and are not modified by the run.

TODO: how to make an exception in the general case? We can specify that
a dependency is writable but what about source code/config and runtime
files? We need a general facility to exempt a file from read-only
status.

The run manifest is written so that tools can rely on a list of input
files (source code, dependencies, and runtime).

Note that files are writable prior to finalizing.

    >>> ls(run.run_dir, permissions=True)  # +diff
    -rw-rw-r-- .venv/bin/activate
    -rw-rw-r-- conf/eval.yaml
    -rw-rw-r-- conf/train.yaml
    -rw-rw-r-- eval.py
    -rw-rw-r-- gage.toml
    -rw-rw-r-- msg.txt
    -rw-rw-r-- setup.py
    -rw-rw-r-- train.py

Finalize the staged run.

    >>> finalize_staged_run(run)

The run files are read only.

    >>> ls(run.run_dir, permissions=True)  # +diff
    -r--r--r-- .venv/bin/activate
    -r--r--r-- conf/eval.yaml
    -r--r--r-- conf/train.yaml
    -r--r--r-- eval.py
    -r--r--r-- gage.toml
    -r--r--r-- msg.txt
    -r--r--r-- setup.py
    -r--r--r-- train.py

Show the run manifest.

    >>> cat(run_meta_path(run, "manifest"))  # +parse
    s {:sha256} conf/eval.yaml
    s {:sha256} conf/train.yaml
    s {:sha256} eval.py
    s {:sha256} gage.toml
    s {:sha256} msg.txt
    s {:sha256} setup.py
    s {train_sha:sha256} train.py
    r {:sha256} .venv/bin/activate

The manifest SHA digest for files is generated after the application of
config.

    >>> assert train_sha == sha256(path_join(run.run_dir, "train.py"))

    >>> assert train_sha != sha256(path_join(sourcecode_dir, "train.py"))

List meta runs.

    >>> ls(run.meta_dir, permissions=True)  # +diff
    -r--r--r-- __schema__
    -r--r--r-- config.json
    -r--r--r-- id
    -r--r--r-- initialized
    -rw-rw-r-- log/files
    -r--r--r-- log/patched
    -rw-rw-r-- log/runner
    -rw-rw-r-- manifest
    -r--r--r-- opdef.json
    -r--r--r-- opref
    -r--r--r-- output/10_sourcecode
    -r--r--r-- output/10_sourcecode.index
    -r--r--r-- output/20_runtime
    -r--r--r-- output/20_runtime.index
    -r--r--r-- proc/cmd.json
    -r--r--r-- proc/env.json
    -r--r--r-- staged

`log/files`, `log/runner`, and `manifest` are left writeable as these
are modified when the staged run is started.

Show logged events.

    >>> cat_log(run_meta_path(run, "log", "runner"))  # +wildcard
    Writing meta id
    ...
    Finalizing staged files (see manifest)
    Writing meta staged

## Errors

    >>> # +skiprest TODO reinstate

Stage a non-existing run directory.

    >>> missing_run_dir = make_temp_dir()
    >>> delete_temp_dir(missing_run_dir)

    >>> stage_run(Run("", missing_run_dir, ""))  # +parse
    Traceback (most recent call last):
    FileNotFoundError: Run dir does not exist: {:path}

Stage a run without a meta dir.

    >>> run_dir_no_meta = make_temp_dir()
    >>> stage_run(Run("", run_dir_no_meta, ""))  # +parse
    Traceback (most recent call last):
    FileNotFoundError: Run meta dir does not exist: {:path}.meta
