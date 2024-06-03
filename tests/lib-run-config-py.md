# Python Config

Python run config is configuration defined in a Python module. Gage can
read and write configuration for Python modules while preserving
comments and whitespace.

Consider a script `hello.py`.

``` python
# hello.py

name = "Joe"
print(f"Hello {name}")
```

The `name` variable represents the script configuration. The value
`Joe`is used by default and is used if otherwise not specified by a flag
for the `run` command.

If the user changed `name` using a flag, Gage rewrites the script to
contain the new name value. This is called the *application of run
configuration*.

Let's say the user runs the command `gage run hello.py name="Mike"`.
When Gage stages the run, it copies the unmodified source code file
`train.py` according to its default source code select rules. Then it
applies run configuration by modifying the copied version of `hello.py`
to be:

``` python
# hello.py

name = "Mike"
print(f"Hello {name}")
```

When the script runs, it prints the message "Hello Mike".

This scheme is applied to any supported configuration file type. In
addition to Python source code files, Gage supports JSON, YAML, and TOML
formats. New formats are added as Gage expands (e.g. R, Julia, etc.)

Note that Gage is language independent in terms of what is run. The
application of configuration to source code files, however, is dependent
on support for specific file formats.

Create a helper function to print config.

    >>> def print_config(config):
    ...     if not config:
    ...         print("<none>")
    ...     else:
    ...         for key, val in sorted(config.items()):
    ...             print(f"{key}: {val}")

## Overview

Python config support is provided by `run_config_py`.

    >>> from gage._internal.run_config_py import *

A Python config file is a Python module that defines configuration
variables as top-level name assignments.

Create a sample Python script `greet.py`.

    >>> source = """
    ... name = "Joe"
    ... print(f"Hello {name}")
    ... """.strip()

    >>> cd(make_temp_dir())

    >>> write("greet.py", source)

Run the script.

    >>> run("python greet.py")
    Hello Joe
    <0>

Create configuration for the source.

    >>> config = PythonConfig(source)

The configuration has the source `name` value.

    >>> print_config(config)
    name: Joe

Apply a new value to the configuration.

    >>> config["name"] = "Mike"

Call `apply()` to apply the new configuration.

    >>> print(config.apply())
    name = "Mike"
    print(f"Hello {name}")

Use `apply()` to update the script and re-run it.

    >>> write("greet.py", config.apply())

    >>> run("python greet.py")
    Hello Mike
    <0>

## Keys

Keys correspond to top-level assignments of config values. Keys are
comprised of the top-level variable and nested keys separated by dots.

Variables must be assigned values to be considered keys. A config value
is a string, number, or boolean. Config values may be contained in lists
and dictionaries.

    >>> config = PythonConfig("""
    ... i = 1
    ... f = 1.0
    ... b = True
    ... s = "Hello"
    ... n = None
    ... l = [1, 1.0, "Hello", False]
    ... d = {
    ...     "i": 1,
    ...     "f": 1.0,
    ...     "s": "Hi",
    ...     "b": False
    ... }
    ... """.strip())

    >>> print_config(config)  # +diff
    b: True
    d.b: False
    d.f: 1.0
    d.i: 1
    d.s: Hi
    f: 1.0
    i: 1
    l.0: 1
    l.1: 1.0
    l.2: Hello
    l.3: False
    n: None
    s: Hello

Apply different configuration.

    >>> config.update({
    ...     "i":  11,
    ...     "f": 2.22,
    ...     "s": "Hola",
    ...     "l.1": 22.44,
    ...     "l.2": "Buenos días",
    ...     "d.b": True,
    ...     "d.s": "Buenas",
    ... })

    >>> print(config.apply())  # +diff
    i = 11
    f = 2.22
    b = True
    s = "Hola"
    n = None
    l = [1, 22.44, "Buenos días", False]
    d = {
        "i": 1,
        "f": 1.0,
        "s": "Buenas",
        "b": True
    }

New keys cannot be added to config.

    >>> config["Z"] = "This value won't appear in applied config"
    Traceback (most recent call last):
    KeyError: 'Z'

Assignments inside functions and class defs are not treated as keys.

    >>> print_config(PythonConfig("""
    ... def foo():
    ...     x = 1
    ...
    ... class Foo:
    ...     y = 2
    ...
    ...     def __init__(self):
    ...         self.z = 3
    ... """))
    <none>

Assignments to tuples are not treated as config value assignments.

    >>> print_config(PythonConfig("""
    ... x, y = 1, 2
    ... """))
    <none>

Type-annotations are supported when reading keys.

    >>> print_config(PythonConfig("""
    ... x: int = 123
    ... y: int | float | None = 1.123
    ... """))
    x: 123
    y: 1.123

## Values

Python config supports these value types: int, float, string, boolean,
and None.

    >>> print_config(PythonConfig("""
    ... a = 1
    ... b = 1.1
    ... c = "abc"
    ... d = True
    ... e = False
    ... f = None
    ... """))
    a: 1
    b: 1.1
    c: abc
    d: True
    e: False
    f: None

Minus operator:

    >>> print_config(PythonConfig("""
    ... a = -1
    ... b = -1.1
    ... c = -True
    ... d = -False
    ... """))
    a: -1
    b: -1.1
    c: -1
    d: 0

Plus operator:

    >>> print_config(PythonConfig("""
    ... a = +1
    ... b = +1.1
    ... c = +True
    ... d = +False
    ... """))
    a: 1
    b: 1.1
    c: 1
    d: 0

Bit invert operator:

    >>> print_config(PythonConfig("""
    ... a = ~1
    ... b = ~True
    ... c = ~False
    ... """))
    a: -2
    b: -2
    c: -1

Various float notation:

    >>> print_config(PythonConfig("""
    ... a = 1e2
    ... b = 1.2e2
    ... c = 1e-2
    ... d = 1.2e-2
    ... """))
    a: 100.0
    b: 120.0
    c: 0.01
    d: 0.012

## Applying changes

Create a function to apply config values to Python source.

    >>> def apply_config(source, vals):
    ...     cfg = PythonConfig(source)
    ...     cfg.update(vals)
    ...     print(cfg.apply())

Integers:

    >>> apply_config("x = 1", {"x": 2})
    x = 2

    >>> apply_config("x = -1", {"x": 2})
    x = 2

    >>> apply_config("x = -1", {"x": -2})
    x = -2

Floats:

    >>> apply_config("x = 1.0", {"x": 1.1})
    x = 1.1

    >>> apply_config("x = -1.1", {"x": 1.2})
    x = 1.2

    >>> apply_config("x = -1.2", {"x": -1.3})
    x = -1.3

String:

    >>> apply_config("x = 'Hello'", {"x": "Goodbye"})
    x = 'Goodbye'

    >>> apply_config("x = \"Hello\"", {"x": "Goodbye"})
    x = "Goodbye"

Booleans:

    >>> apply_config("x = False", {"x": True})
    x = True

Gage supports replacements with any type. Type validation must be
applied to values prior to their application.

    >>> apply_config("x = 1", {"x": 1.1})
    x = 1.1

    >>> apply_config("x = 1", {"x": "Hello"})
    x = 'Hello'

    >>> apply_config("x = None", {"x": -123})
    x = -123

    >>> apply_config("x = None", {"x": "Goodbye"})
    x = 'Goodbye'

Type annotations are preserved.

    >>> apply_config("x: int = 1", {"x": 2})
    x: int = 2

However, it's possible to assign a value of an invalid type. Type
validation must be applied to values prior to application.

    >>> apply_config("x: int = 1", {"x": "😓"})
    x: int = '😓'

## Preserving comments and whitespace

Python config preserves comments whitespace across applications.

    >>> source = """
    ... import something
    ...
    ... # Hyper params
    ... x = 1
    ... y = 2
    ...
    ... # Derived
    ... z = x + 1
    ...
    ... # Training loop
    ... while True:
    ...     train(x, y)  # Where the magic happens
    ...
    ... # Results
    ... print(f"loss: {loss()}")
    ... """.strip()

    >>> config = PythonConfig(source)

Without changes to config, the source is preserved.

    >>> assert source == config.apply()

Changes to config only affect the applied values.

    >>> config["x"] = 123
    >>> config["y"] = 765
    >>> applied = config.apply()

    >>> udiff(source, applied, 0)  # not sure but we need -space here
    ---
    +++
    @@ -4,2 +4,2 @@
    -x = 1
    -y = 2
    +x = 123
    +y = 765
