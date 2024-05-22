# Gage File Samples

Use `check` to validate sample Gage files.

    >>> cd(sample("gagefiles"))

Use `load_gagefile()` to load a Gage file.

    >>> from gage._internal.gagefile import load_gagefile

## Empty file

An empty file is invalid JSON and can't be loaded.

    >>> run("gage check empty.json")
    gage: Error loading empty.json: Expecting value: line 1 column 1 (char 0)
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

    >>> train.get_exec().get_finalize()
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

    >>> run("gage check writeable-dependencies.json")  # +fails TODO implement deps
    writeable-dependencies.json is a valid Gage file
    <0>

    >>> run("gage check writeable-dependencies.json")  # TODO implement deps
    There are errors in writeable-dependencies.json
    Properties ['train'] are invalid
    ['requires']
    <1>

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

## Config

Gage supports a variety of config specs.

    >>> run("gage check config.toml")
    config.toml is a valid Gage file
    <0>

## Kitchen sink

`kitchen-sink.json` is intended to demonstrate a variety of
configurations.

    >>> run("gage check kitchen-sink.toml")
    kitchen-sink.toml is a valid Gage file
    <0>

## JSON with comments

Comments are supported but must be on their own line.

    >>> run("gage check jsonc-valid.json")
    jsonc-valid.json is a valid Gage file
    <0>

    >>> run("gage check jsonc-invalid.json")  # -space
    gage: Error loading jsonc-invalid.json: Expecting ',' delimiter: line 2
    column 15 (char 16)
    <1>

## Runs dir

A project can configure a runs directory location with `$runs-dir`.

    >>> run("gage check runs-dir.json")
    runs-dir.json is a valid Gage file
    <0>

## Progress specs

Project specs can be a single string or a mapping that corresponds to
the run phase execs.

    >>> run("gage check progress-specs.toml")
    progress-specs.toml is a valid Gage file
    <0>

    >>> run("gage check invalid-progress.json")  # +wildcard
    There are errors in invalid-progress.json
    Properties ['op', 'op-2', 'op-3'] are invalid
    Properties ['progress'] are invalid
    ...
    The instance must be of type "string"
    The instance must be of type "object"
    Properties ['progress'] are invalid
    ...
    The instance must be of type "string"
    ['not-supported']
    Properties ['progress'] are invalid
    ...
    The instance must be of type "string"
    Properties ['run'] are invalid
    The instance must be of type "string"
    <1>

## Listing descriptions

The description used in run lists can be configured using an operation
listing description.

    >>> run("gage check listing-description.json")
    listing-description.json is a valid Gage file
    <0>

    >>> run("gage check invalid-listing-description.json")
    There are errors in invalid-listing-description.json
    Properties ['op', 'op-2', 'op-3'] are invalid
    Properties ['listing'] are invalid
    Properties ['description'] are invalid
    [0]
    The instance must be of type "string"
    Properties ['listing'] are invalid
    ['foo']
    Properties ['listing'] are invalid
    The instance must be of type "object"
    <1>

## Output Summary

    >>> run("gage check output-summary.json")
    output-summary.json is a valid Gage file
    <0>

    >>> run("gage check invalid-output-summary.json")
    There are errors in invalid-output-summary.json
    Properties ['op'] are invalid
    Properties ['output-summary'] are invalid
    The instance must be valid against exactly one subschema; it is valid against [] and invalid against [0, 1]
    The instance must be of type "boolean"
    The instance must be of type "string"
    <1>

## Op Version

    >>> run("gage check op-version.json")
    op-version.json is a valid Gage file
    <0>

    >>> run("gage check invalid-op-version.json")
    There are errors in invalid-op-version.json
    Properties ['op-1', 'op-2', 'op-3'] are invalid
    Properties ['version'] are invalid
    The instance must be valid against exactly one subschema; it is valid against [] and invalid against [0, 1]
    The instance must be of type "string"
    The instance must be of type "null"
    Properties ['version'] are invalid
    The instance must be valid against exactly one subschema; it is valid against [] and invalid against [0, 1]
    The text must match the regular expression "^[^\\s]*$"
    The instance must be of type "null"
    Properties ['version'] are invalid
    The instance must be valid against exactly one subschema; it is valid against [] and invalid against [0, 1]
    The text is too short (minimum 1 characters)
    The instance must be of type "null"
    <1>
