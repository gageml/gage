# Run config support

`run_config` provides support for run configuration implementation.

    >>> from gage._internal.run_config import *
    >>> from gage._internal.types import *

## RunConfig base class

The class `RunConfigBase` is an abstract class that provides dict
support for format-specific run config.

    >>> config = RunConfig()

Concrete classes should implement config init in their constructors and
implement the `apply()` method.

    >>> config.apply()
    Traceback (most recent call last):
    NotImplementedError

Otherwise, the class provides a dict-like interface for setting and
reading configuration.

    >>> config["x"] = 123
    >>> config["y"] = 456
    >>> config.update({"z": 789})

    >>> config  # +pprint
    {'x': 123, 'y': 456, 'z': 789}

    >>> config["x"]
    123

    >>> config["a"]
    Traceback (most recent call last):
    KeyError: 'a'

When initialized, `_initialized` should set to to `True` to prevent new
config keys form being added.

    >>> config._initialized = True

    >>> config["x2"] = 321
    Traceback (most recent call last):
    KeyError: 'x2'

Existing keys can be updated.

    >>> config["x"] = 321

    >>> config  # +pprint
    {'x': 321, 'y': 456, 'z': 789}

## Matching keys

`match_keys()` applies a list of include and exclude patterns to a list
of keys and return the corresponding list of matching keys.

Matching rules are:

- `*` matches one level of keys and can be used with other characters
- `**` matches any level of keys
- Otherwise characters and dots are matched as specified

Empty include list never matches.

    >>> match_keys([], [], [])
    []

    >>> match_keys([], [], ["a"])
    []

Use `*` to match one level.

    >>> match_keys(["*"], [], ["a"])
    ['a']

    >>> match_keys(["*"], [], ["a", "b"])
    ['a', 'b']

    >>> match_keys(["*"], [], ["a", "b", "c.d"])
    ['a', 'b']

    >>> match_keys(["c.*"], [], ["a", "b", "c.d"])
    ['c.d']

`*` may be used with other characters to match within a level.

    >>> match_keys(["a*"], [], ["aaa", "aba", "bab", "bcb"])
    ['aaa', 'aba']

    >>> match_keys(["b*"], [], ["aaa", "aba", "bab", "bcb"])
    ['bab', 'bcb']

    >>> match_keys(["*a"], [], ["aaa", "aba", "bab", "bcb"])
    ['aaa', 'aba']

    >>> match_keys(["*c*"], [], ["aaa", "aba", "bab", "bcb"])
    ['bcb']

`**` matches any level but must be used in conjunction with another
pattern.

    >>> match_keys(["**"], [], ["a"])
    []

    >>> match_keys(["**.*"], [], ["a"])
    ['a']

    >>> match_keys(["**.*"], [], ["a", "a.b", "a.b.c", "a.b.c.d"])
    ['a', 'a.b', 'a.b.c', 'a.b.c.d']

    >>> match_keys(["**.b"], [], ["a", "a.b", "a.b.c", "a.b.c.d"])
    ['a.b']

    >>> match_keys(["**.c"], [], ["a", "a.b", "a.b.c", "a.b.c.d"])
    ['a.b.c']

    >>> match_keys(["a.**.*"], [], ["a", "a.b", "a.b.c", "a.b.c.d"])
    ['a.b', 'a.b.c', 'a.b.c.d']

    >>> match_keys(["**.c.**.*"], [], ["a", "a.b", "a.b.c", "a.b.c.d"])
    ['a.b.c.d']

Exclude matches apply the same rules but remove matches.

    >>> match_keys(["a"], ["a"], ["a"])
    []

    >>> match_keys(["*"], ["a"], ["a"])
    []

    >>> match_keys(["*"], ["a"], ["a", "b"])
    ['b']

    >>> match_keys(["a.**.*"], ["**.d"], ["a", "a.b", "a.b.c", "a.b.c.d"])
    ['a.b', 'a.b.c']

Multiple includes extend the selected results.

    >>> match_keys(["a", "a.**.*"], [], ["a", "a.b", "a.b.c", "a.b.c.d"])
    ['a', 'a.b', 'a.b.c', 'a.b.c.d']

Multiple excludes extend the excluded results.

    >>> match_keys(["a.**.*"], ["**.d", "**.c"],
    ...            ["a", "a.b", "a.b.c", "a.b.c.d"])
    ['a.b']

## Parsing paths

The private function `_parse_path()` converts a a path into
`ParsedPath`.

    >>> from gage._internal.run_config import _parse_path

A parsed path consists of four parts: file pattern, file excluded flag,
key pattern, and key excluded flag.

Parse a path with both a file and key part.

    >>> _parse_path("train.py#x")  # -space
    ParsedPath(file_pattern='train.py',
               file_exclude=False,
               key_pattern='x',
               key_exclude=False)

A file part without a key part implies '*' for the key (matches
top-level keys).

    >>> _parse_path("train.py")  # -space
    ParsedPath(file_pattern='train.py',
               file_exclude=False,
               key_pattern='*',
               key_exclude=False)

A key part without a file part implies no file-select pattern. This path
is not used to select files but is used to select keys.

    >>> _parse_path("#x")  # -space
    ParsedPath(file_pattern=None,
               file_exclude=False,
               key_pattern='x',
               key_exclude=False)

When a key part is specified in an excluded path, the file is not
excluded. The excluded flag applies only to matched keys.

    >>> _parse_path("-train.py#x")  # -space
    ParsedPath(file_pattern='train.py',
               file_exclude=False,
               key_pattern='x',
               key_exclude=True)

When a file is excluded without a key part, the file is excluded as
expected.

    >>> _parse_path("-train.py")  # -space
    ParsedPath(file_pattern='train.py',
               file_exclude=True,
               key_pattern=None,
               key_exclude=True)

When a key part is excluded, the pattern is not applied to file
selection but is used to remove keys from matching config.

    >>> _parse_path("-#x")  # -space
    ParsedPath(file_pattern=None,
               file_exclude=False,
               key_pattern='x',
               key_exclude=True)

Paths can't be empty.

    >>> _parse_path("")
    Traceback (most recent call last):
    ValueError: path cannot be empty

## Selecting config files

The private function `_select_files()` selects files from a directory
given a list of parsed paths. See **Parsing paths** above for details
about parsed paths.

    >>> from gage._internal.run_config import _select_files

Create a sample directory structure.

    >>> target_dir = make_temp_dir()
    >>> cd(target_dir)

    >>> write("op.py", """
    ... x = 1
    ... y = 2
    ... op = "+"
    ... expr = f"{x} {op} {y}"
    ... print(f"{expr} = {eval(expr)}")
    ... """.lstrip())

    >>> write("greet.py", """
    ... name = "Bob"
    ... opts = {
    ...     "loud": False,
    ...     "short": True
    ... }
    ... if opts['loud']:
    ...     name = name.upper()
    ... greeting = "Hi" if opts["short"] else "Hello"
    ... print(f"{greeting} {name}")
    ... """.lstrip())

    >>> write("config.json", """
    ... {
    ...     "a": 3,
    ...     "b": 4
    ... }
    ... """.lstrip())

    >>> touch("sample.txt")
    >>> touch("sample.bin")

    >>> ls(target_dir)
    config.json
    greet.py
    op.py
    sample.bin
    sample.txt

Create a function to print selected paths from `target_dir` for a list
of config paths.

    >>> def select_files(paths):
    ...     any = False
    ...     parsed = [_parse_path(path) for path in paths]
    ...     for path in sorted(_select_files(target_dir, parsed)):
    ...         any = True
    ...         print(path)
    ...     if not any:
    ...         print("<none>")

    >>> select_files([])
    <none>

    >>> select_files(["greet.py"])
    greet.py

    >>> select_files(["greet.py", "-greet.py"])
    <none>

    >>> select_files(["*.py", "*.txt"])
    greet.py
    op.py
    sample.txt

    >>> select_files(["*.*", "-*.txt", "-op*"])
    config.json
    greet.py
    sample.bin

Excluding paths still includes selected files. In this case, keys are
excluded but files they apply to are not.

    >>> select_files(["*.py", "-greet.py#msg"])
    greet.py
    op.py

    >>> select_files(["op.py", "*.json", "-#y"])
    config.json
    op.py

## Reading file config

The function `read_file_config()` loads config for file. If the file
format is not supported, the function raises `UnsupportedFileFormat`.

    >>> read_file_config(path_join(target_dir, "greet.py"))  # +pprint
    {'name': 'Bob', 'opts.loud': False, 'opts.short': True}

    >>> read_file_config(path_join(target_dir, "op.py"))  # +pprint
    {'op': '+', 'x': 1, 'y': 2}

    >>> read_file_config(path_join(target_dir, "config.json"))  # +parse
    Traceback (most recent call last):
    gage._internal.run_config.UnsupportedFileFormat: {:path}/config.json

Raises `FileNotFoundError` if the specified file doesn't exist.

    >>> read_file_config(path_join(target_dir, "not-here.py"))  # +parse
    Traceback (most recent call last):
    FileNotFoundError: [Errno 2] No such file or directory: '{:path}/not-here.py'

## Selecting keys

The private function `_selected_keys()` returns a list of config keys
that apply to a list of selected files.

    >>> from gage._internal.run_config import _select_keys

`_select_keys()` requires the following:

- A list of selected files (see **Selecting config files** above) and
  associated config (see **Loading config** above)
- A list of of parsed paths (see **Parsing paths** above)

Create a function to select keys.

    >>> def select_keys(paths):
    ...     parsed = [_parse_path(path) for path in paths]
    ...     selected = []
    ...     for path in _select_files(target_dir, parsed):
    ...         try:
    ...             file_config = read_file_config(path_join(target_dir, path))
    ...         except Exception as e:
    ...             print(f"WARNING: {e}")
    ...         else:
    ...             selected.append((path, file_config))
    ...     print(sorted(_select_keys(target_dir, selected, parsed)))

Without a pattern, nothing is selected.

    >>> select_keys([])
    []

If no files match, nothing is selected.

    >>> select_keys(["not-matching-pattern"])
    []

By default, top-level keys are selected for a file.

    >>> select_keys(["op.py"])
    ['op', 'x', 'y']

    >>> select_keys(["greet.py"])
    ['name']

Individual keys can be selected.

    >>> select_keys(["op.py#x"])
    ['x']

    >>> select_keys(["op.py#op"])
    ['op']

    >>> select_keys(["p[.py#x", "op.py#y"])
    ['x', 'y']

An empty key does not select keys.

    >>> select_keys(["op.py#"])
    []

An empty key, however, selects the file, to which other patterns apply.

    >>> select_keys(["op.py#", "#x"])
    ['x']

    >>> select_keys(["op.py#", "#x", "#y"])
    ['x', 'y']

Wildcards are used to select multiple files.

    >>> select_keys(["*.py"])
    ['name', 'op', 'x', 'y']

Wildcards in file patterns can be used with keys.

    >>> select_keys(["*.py#x"])
    ['x']

Keys may also include wildcards.

    >>> select_keys(["*.py#*"])
    ['name', 'op', 'x', 'y']

`**` used in a key pattern selects nested keys.

    >>> select_keys(["*.py#**.*"])
    ['name', 'op', 'opts.loud', 'opts.short', 'x', 'y']

    >>> select_keys(["*.py#**.loud"])
    ['opts.loud']

To exclude a file, use a `-` prefix.

    >>> select_keys(["*.py", "-op.py"])
    ['name']

Other examples:

    >>> select_keys(["op.py", "-op.py#op", "greet.py"])
    ['name', 'x', 'y']

    >>> select_keys([
    ...     "op.py",
    ...     "-op.py#op",
    ...     "greet.py#opts.*",
    ...     "-#opts.loud"
    ... ])
    ['name', 'opts.short', 'x', 'y']

## Applying configuration

`apply_config()` orchestrates the functions above to apply configuration
to a destination directory according to opdef config paths.

Create a function that copies `target_dir` and applies config to its
files according to opdef config paths. It prints the diffs of applied
config.

    >>> def apply(config, keys):
    ...     copy_dir = make_temp_dir()
    ...     copytree(target_dir, copy_dir)
    ...     opdef = OpDef("test", {
    ...         "config": {
    ...             "keys": keys
    ...         }
    ...     })
    ...     diffs = apply_config(config, opdef, copy_dir)
    ...     for path, diff in sorted(diffs):
    ...         for line in diff:
    ...             print(line, end="")
    ...     return copy_dir

Change name used in `greet.py`

    >>> applied = apply({"name": "Mary"}, ["greet.py"])
    --- greet.py
    +++ greet.py
    @@ -1,4 +1,4 @@
    -name = "Bob"
    +name = "Mary"
     opts = {
         "loud": False,
         "short": True

Compare the original and modified versions of `greet.py`.

    >>> diff(path_join(target_dir, "greet.py"), path_join(applied, "greet.py"))
    @@ -1,4 +1,4 @@
    -name = "Bob"
    +name = "Mary"
     opts = {
         "loud": False,
         "short": True

Run the modified Python script.

    >>> run("python greet.py", cwd=applied)
    Hi Mary
    <0>

Change name, loudness, and shortness.

    >>> applied = apply({
    ...     "name": "Jane",
    ...     "opts.loud": True,
    ...     "opts.short": False
    ... }, ["greet.py#**.*"])
    --- greet.py
    +++ greet.py
    @@ -1,7 +1,7 @@
    -name = "Bob"
    +name = "Jane"
     opts = {
    -    "loud": False,
    -    "short": True
    +    "loud": True,
    +    "short": False
     }
     if opts['loud']:
         name = name.upper()

    >>> run("python greet.py", cwd=applied)
    Hello JANE
    <0>

Config that doesn't apply to a file is ignored.

    >>> applied = apply({"does-not-apply": 123}, ["greet.py"])

    >>> diff(path_join(target_dir, "greet.py"), path_join(applied, "greet.py"))

Missing config is ignored.

    >>> applied = apply({}, ["greet.py"])

    >>> diff(path_join(target_dir, "greet.py"), path_join(applied, "greet.py"))

If applied config doesn't change a file because the values are
unchanged, the file isn't modified.

    >>> applied = apply({"name": "Bob", "op": "+", "x": 1}, ["*.py"])

    >>> ls(applied)
    config.json
    greet.py
    op.py
    sample.bin
    sample.txt

    >>> run("python op.py", cwd=applied)
    1 + 2 = 3
    <0>

Run a modified `op.py` script.

    >>> applied = apply({"op": "-", "x": 10, "y": 7}, ["*.py"])
    --- op.py
    +++ op.py
    @@ -1,5 +1,5 @@
    -x = 1
    -y = 2
    -op = "+"
    +x = 10
    +y = 7
    +op = "-"
     expr = f"{x} {op} {y}"
     print(f"{expr} = {eval(expr)}")

    >>> run("python op.py", cwd=applied)
    10 - 7 = 3
    <0>

## Reading project configuration

`read_project_config()` reads configuration from a source directory
given op def settings.

Create a directory with configuration sources.

    >>> cd(make_temp_dir())

    >>> write("test.py", """
    ... a = 1
    ... b = "Hello"
    ... c = {
    ...   "d": 123,
    ...   "e": [6, 7, 8],
    ...   "f": {
    ...     "g": 1.123
    ...   }
    ... }
    ... """)


Create an op def that includes `hello.py` as config.

    >>> opdef = OpDef("test", {
    ...   "config": "test.py"
    ... })

Read the configuration from the directory.

    >>> config = read_project_config(".", opdef)

    >>> config  # +pprint
    {'a': 1,
     'b': 'Hello',
     'c.d': 123,
     'c.e.0': 6,
     'c.e.1': 7,
     'c.e.2': 8,
     'c.f.g': 1.123}

## To Do / Notes

TODO: namespace maybe wants to be 'prefix'.

TODO: want to strip prefix - so maybe 'strip-prefix' or similar -- e.g.
to expose deeply nested config

TODO: maybe a 'rename', which is short hand for prefix/strip-prefix

For example:

``` toml
[a.config]

path = "config.json#resnet50.train.hparams.*"
prefix = "train."
strip-prefix = "resnet50.train.hparams."
```

Could be written as:

``` toml
[a.config]

path = "config.json#resnet50.train.hparams.*"
rename = "resnet50.train.hparams. train."
```

Rename could be a list.

``` toml
[a.config]

path = "train.py"
rename = ["x X", "y Y"]
```

Rename could used on a single key.

``` toml
[a.config]

path = "train.py#x"
rename = ["x X"]  # equiv to prefix = "X" + strip-prefix = "x"
```

TODO: Edge case: two config files with the same key, different values -
show how they'll end up with the same value based on `config.json`,
which may be surprising.

TODO: Show how namespace is used to deal with that edge case.

TODO: Application of config needs to be per `config` section due to
namespace potential.

TODO: Note somewhere the idea of a *path prefix*, which is a config
setting that indicates that a path-based prefix should be used with
selected keys. This would be handle for Hydra main config, which is
setup with a `config_path`, under which files and subdirectories form
key prefixes.

`path-prefix-root` specifies the root directory from which prefixes are
applied to selected keys.

``` toml
[a.config]

path = "conf/**.*#**.*"
path-prefix-root = "conf"
```

This is a feature we can add later. There's a chance this won't be
useful in practice as a Hydra scheme is for huge, complex config that's
not likely all exposed as operation config. E.g. a MySQL port or
selection of database is not something a model training operation cares
about tracking. These can be passed along as args using `gage run ... --
ARGS` as needed and applied as config via Hydra and not something Gage
cares to track.

OTOH, learning rate for a model training run is something the user cares
about. But this can be more specifically exposed via op config.

``` toml
[a.config]

path = "conf/resnet50/train.yaml"
```
