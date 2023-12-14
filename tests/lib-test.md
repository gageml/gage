---
test-options: +parse
---

# gage test support

## Pattern matching

### Semantic versions

Use `semver` to match valid semantic versions.

    >>> ["0.1.2", "2.0.10", "0.0.0-rc.2"]
    ['{:ver}', '{:ver}', '{:ver}']

Failure cases:

    >>> "0.0.0.1"  # +fails
    {:semver}

### Paths

Paths can be matched with the `path` type.

    >>> "/usr/bin/git"
    '{:path}'

Paths must be absolute to match.

    >>> "bin/git"  # +fails
    '{:path}'

### Any value

`any` is used to match anything within the same line. This is an
important distinction from `{}`, which matches across lines. It's
important to avoid `{}` when it might consume output that would
otherwise negate a successful test.

The following test makes this mistake:

    >>> print("""SUCCESS: well done!
    ...
    ... ERROR: I was joking earlier""" )
    SUCCESS: {}

The example should use `any`.

    >>> print("""SUCCESS: well done!
    ...
    ... ERROR: I was joking earlier""")  # +fails
    SUCCESS: {:any}

### Dates

ISO 8601 formats:

    >>> print("2023-09-03T11:21:33-0500")
    {:isodate}

    >>> print("2023-09-03T11:21:33+0500")
    {:isodate}

    >>> print("2023-09-03T11:21:33+050030")
    {:isodate}

Valid formats but not supported:

    >>> print("2023-09-03T11:21:33-05:00")  # +fails
    {:isodate}

    >>> print("2023-09-03T11:21:33-05:00:30")  # +fails
    {:isodate}

### Timestamps

The `timestamp` pattern matches epoch microsecond timestamps.

    >>> import time

    >>> print(int(time.time() * 1000000))  # +parse
    {:timestamp}

`timestamp_ms` matches epoch millisecond timestamps.

    >>> print(int(time.time() * 1000))  # +parse
    {:timestamp_ms}

`timestamp_s` matches epoch seconds timestamps.

    >>> print(int(time.time()))  # +parse
    {:timestamp_s}

## `cat_json`

`cat_json` prints a JSON file as formatted JSON.

    >>> tmp = make_temp_dir()
    >>> write(path_join(tmp, "test.json"), """{"z": 123, "a": [1, 2, "abc"]}""")

    >>> cat_json(path_join(tmp, "test.json"))
    {
      "a": [
        1,
        2,
        "abc"
      ],
      "z": 123
    }
