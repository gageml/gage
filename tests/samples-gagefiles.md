# Gage file samples

Use `check` to validate sample Gage files.

    >>> cd(sample("gagefiles"))

Use `load_gagefile()` to load a Gage file.

    >>> from gage._internal.gagefile import load_gagefile

## Empty file

An empty file is invalid JSON and can't be loaded.

    >>> run("gage check empty.json")
    Error loading empty.json: Expecting value: line 1 column 1 (char 0)
    <1>

## Empty object

An empty object is a valid Gage file.

    >>> run("gage check object.json")
    object.json is a valid Gage file
    <0>

## Minimal configuration

A minimal configuration is one operation with an exec attribute.

    >>> run("gage check minimal.json")
    minimal.json is a valid Gage file
    <0>

    >>> gf = load_gagefile("minimal.json")

    >>> gf.get_operations()  # +wildcard
    {'train': <gage._internal.types.OpDef ...>}

    >>> train = gf.get_operations()["train"]

    >>> train.get_exec().get_run()
    'echo hello'

## Missing exec

An operation does not need an exec attribute.

    >>> run("gage check missing-exec.json")
    missing-exec.json is a valid Gage file
    <0>

    >>> gf = load_gagefile("missing-exec.json")

    >>> gf.get_operations()["train"].get_exec().get_run()

## Full exec spec

A full exec spec uses an object for `exec` to define commands for
various stages of a run lifecycle.

    >>> run("gage check full-exec.json")
    full-exec.json is a valid Gage file
    <0>

    >>> gf = load_gagefile("full-exec.json")

    >>> gf.get_operations()  # +wildcard
    {'train': <gage._internal.types.OpDef ...>}

    >>> train = gf.get_operations()["train"]

    >>> train.get_exec()  # +wildcard
    <gage._internal.types.OpDefExec ...>

    >>> train.get_exec().get_stage_sourcecode()
    'cp * $run_dir'

    >>> train.get_exec().get_stage_dependencies()
    ''

    >>> train.get_exec().get_stage_runtime()
    ['python', '-m', 'venv']

    >>> train.get_exec().get_run()
    'dir .'

    >>> train.get_exec().get_finalize_run()
    ''

## Invalid full exec

Exec commands must be either strings or arrays of strings.

    >>> run("gage check invalid-exec.json")  # +wildcard
    There are errors in invalid-exec.json
    Properties ['train'] are invalid
    Properties ['exec'] are invalid
    ...
    The instance must be of type "string"
    The instance must be of type "array"
    Properties ['stage-dependencies', 'stage-runtime', 'run'] are invalid
    ...
    The instance must be of type "string"
    [0]
    ...
    The instance must be of type "string"
    The instance must be of type "array"
    <1>

## Writeable dependencies

By default Gage sets resolved dependency files to read-only under the
assumption that dependencies are not modified by a run. In cases where a
resolved dependency must be written, the dependency may specify
`writeable` as a boolean or as an array of paths.

    >>> run("gage check writeable-dependencies.json")  # +skip - dependencies pending
    writeable-dependencies.json is a valid Gage file
    <0>

## Missing required keys

`keys` is required.

    >>> run("gage check empty-config.json")  # +wildcard
    There are errors in empty-config.json
    Properties ['train'] are invalid
    Properties ['config'] are invalid
    ...
    The object is missing required properties ['keys']
    ...
    <1>

## Empty config

At a minimum, `files` is required for `depends`.

    >>> run("gage check empty-depends.json")  # +wildcard
    There are errors in empty-depends.json
    Properties ['a'] are invalid
    Properties ['depends'] are invalid
    ...
    The object is missing required properties ['files']
    ...
    <1>

## Kitchen sink

`kitchen-sink.json` is intended to demonstrate a variety of
configurations.

    >>> run("gage check kitchen-sink.toml")
    kitchen-sink.toml is a valid Gage file
    <0>
