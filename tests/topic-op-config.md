---
test-options: +skip until implemented
---

# Operation config

``` toml
[train]

exec = "python train.py"

[train.config]

keys = "train.py"   # shorthand for train.py#*
```

Multiple explicit keys:

``` toml
[train.config]

keys = [
    "train.py#x",
    "train.py#y"
]
```

Wildcard include with specific excludes.

``` toml
[train.config]

keys = [
  "train.py#*",
  "-train.py#x"
]
```

Include a ton of config - all keys in all files ending in `.yaml`. This
is how a scheme like Hydra is supported.

``` toml
[train]

config = "conf/**/*.yaml#**.*"
```

Specific path - can be annotated with description, type, etc:

``` toml
[[train.config]]

description = "Some var x"   # optional

keys = "train.py#x"          # required when name is specified
type = "int"                 # optional, inferred by value
```

Renamed single key:

``` toml
[[train.config]]

prefix = "train."
keys = "train.py#x"
description = "Some var x"
type = "int"
```

Renamed multiple keys:

``` toml
[[train.config]]

prefix = "train."
include = "train.py#*"
```

## `config` command

Command to list, read, and set config values. Can be used to test and
for fun.

### Python scripts

Create a sample Python script.

    >>> cd(make_temp_dir())

    >>> write("test.py", """
    ... msg = "Hello"
    ... n = 1
    ... for i in range(n):
    ...     print(msg if i == 0 else f"{msg} {i + 1}")
    ... """)

    >>> run("python test.py")
    Hello
    <0>

List its config keys.

    >>> run("gage config ls test.py")
    msg
    n
    <0>

Alternative form, use a wildcard key path.

    >>> run("gage config ls test.py#*")
    msg
    n
    <0>

List a specific key.

    >>> run("gage config ls test.py#msg")
    msg
    <0>

Read config for the script.

    >>> run("gage config get test.py")
    {
      "msg": "Hello",
      "n": 1
    }
    <0>

Read specific keys.

    >>> run("gage config get test.py#msg")
    "Hello"

    >>> run("gage config get test.py#n")
    1

Run the script with an alternate value for `n`.

    >>> run("gage run test.py n=3 -y")
    Hello
    Hello 2
    Hello 3

### JSON config

Create a sample JSON file.

    >>> write("test.json", """
    ... {
    ...   "a": 1,
    ...   "b": {
    ...     "c": 2,
    ...     "d": {
    ...       "e": "the bottom"
    ...     }
    ...   }
    ... }
    ... """)

List its keys.

    >>> run("gage config ls test.json")
    a
    b.c
    b.d.e

    >>> run("gage config ls test.json#*")
    a

    >>> run("gage config ls test.json#**.*")
    a
    b.c
    b.d.e

    >>> run("gage config ls test.json#b.*")
    b.c

    >>> run("gage config ls test.json#b.**.*")
    b.c
    b.d.e

Read config values.

    >>> run("gage config get test.json#b")
    {
      "c": 2,
      "d": {
        "e": "the bottom"
      }
    }

    >>> run("gage config get test.json#b.d")
    {
      "e": "the bottom"
    }

    >>> run("gage config get test.json#b.d.e")
    "the bottom"

Modify test.json.

    >>> run("gage config set test.json#b.d.e \"it's not so bad\" -o test2.json")
    >>> run("gage config set test2.json#a 123")

    >>> cat("test2.json")
    {
      "a": 123,
      "b": {
        "c": 2,
        "d": {
          "e": "it's not so bad"
        }
      }
    }
