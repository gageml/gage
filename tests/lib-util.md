# Utils

## Matching filters

    >>> from gage._internal.util import match_filters

Empty case:

    >>> match_filters([], [])
    True

One filter for empty vals:

    >>> match_filters(["a"], [])
    False

No filters for vals:

    >>> match_filters([], ["a"])
    True

One filter matching one val:

    >>> match_filters(["a"], ["a"])
    True

One filter not matching one val:

    >>> match_filters(["a"], ["b"])
    False

One filter matching one of two vals:

    >>> match_filters(["a"], ["b", "a"])
    True

Two filters for empty vals:

    >>> match_filters(["a", "b"], [])
    False

Two filters for one matching where match_any is False (default):

    >>> match_filters(["a", "b"], ["a"])
    False

Two filters for one matching val where match_any is True:

    >>> match_filters(["a", "b"], ["a"], match_any=True)
    True

Two filters matching both vals:

    >>> match_filters(["a", "b"], ["a", "b"])
    True

Two filters matching both vals (alternate order);

    >>> match_filters(["a", "b"], ["b", "a"])
    True

## Resolving references

Use `resolve_refs` to resolve references in a string.

    >>> from gage._internal.util import resolve_refs

Empty string:

    >>> resolve_refs("", {})
    ''

Missing ref generates an error:

    >>> resolve_refs("${a}", {})
    Traceback (most recent call last):
    gage._internal.util.UndefinedReferenceError: a

    >>> resolve_refs("foo ${bar} baz", {})
    Traceback (most recent call last):
    gage._internal.util.UndefinedReferenceError: bar

A default may be provided to use for missing values:

    >>> resolve_refs("${a}", {}, "")
    ''

    >>> resolve_refs("foo ${bar} baz", {}, "<missing>")
    'foo <missing> baz'

Single refs are resolved using the applicable typed value:

    >>> resolve_refs("${a}", {"a": "a"})
    'a'

    >>> resolve_refs("${a}", {"a": 123})
    123

    >>> resolve_refs("${a}", {"a": 1.234})
    1.234

    >>> resolve_refs("${a}", {"a": True})
    True

    >>> print(resolve_refs("${a}", {"a": None}))
    None

    >>> print(resolve_refs("${a}", {"a": [1, 'a', True, None]}))
    [1, 'a', True, None]

When used in a string, resolved refs are encoded:

    >>> resolve_refs("foo ${bar} baz", {"bar": "bar"})
    'foo bar baz'

    >>> resolve_refs("foo ${bar} baz", {"bar": 123})
    'foo 123 baz'

    >>> resolve_refs("foo ${bar} baz", {"bar": 1.234})
    'foo 1.234 baz'

    >>> resolve_refs("foo ${bar} baz", {"bar": True})
    'foo true baz'

    >>> resolve_refs("foo ${bar} baz", {"bar": None})
    'foo null baz'

    >>> normlf(resolve_refs(
    ...     "foo ${bar} baz",
    ...     {"bar": [1, 'a', True, None]}))
    'foo - 1\n- a\n- true\n- null baz'

Escaped references aren't resolved:

    >>> resolve_refs("\\${a}", {})
    '${a}'

    >>> resolve_refs("\\${a}", {"a": "a"})
    '${a}'

    >>> resolve_refs("foo \\${bar} baz", {})
    'foo ${bar} baz'

    >>> resolve_refs("foo \\${bar} baz", {"bar": "bar"})
    'foo ${bar} baz'

### `resolve_all_refs`

A map of vals may contain references to other vals. Use
`resolve_all_refs` to resolve all references in the map.

    >>> from gage._internal.util import resolve_all_refs

No references:

    >>> resolve_all_refs({"a": 1})
    {'a': 1}

    >>> resolve_all_refs({"a": "1"})
    {'a': '1'}

Reference to undefined value:

    >>> resolve_all_refs({"a": "${b}"})
    Traceback (most recent call last):
    gage._internal.util.UndefinedReferenceError: b

    >>> resolve_all_refs({"a": "${b}"}, undefined="foo")
    {'a': 'foo'}

Reference to a value:

    >>> resolve_all_refs({"a": "${b}", "b": 1})  # +pprint
    {'a': 1, 'b': 1}

Reference to a value with a reference:

    >>> resolve_all_refs({"a": "${b}", "b": "${c}", "c": 1})  # +pprint
    {'a': 1, 'b': 1, 'c': 1}

Reference embedded in a string:

    >>> resolve_all_refs({"a": "b equals ${b}", "b": 1})  # +pprint
    {'a': 'b equals 1', 'b': 1}

    >>> resolve_all_refs(
    ... {"msg": "${x} + ${y} = ${z}",
    ...  "x": "one",
    ...  "y": "two",
    ...  "z": "three"})["msg"]
    'one + two = three'

Reference cycle:

    >>> resolve_all_refs({"a": "${b}", "b": "${a}"})
    Traceback (most recent call last):
    gage._internal.util.ReferenceCycleError: ['b', 'a', 'b']

Resolving non string values:

    >>> resolve_all_refs({
    ...   "msg": "${i} ${f} ${none}",
    ...   "i": 1,
    ...   "f": 1.2345,
    ...   "none": None})["msg"]
    '1 1.2345 null'

Note that None is resolved as 'null' for consistency with flag inputs,
which convert the string 'null' into None.

A reference can be escaped:

    >>> resolve_all_refs({"a": "\${foo}"})
    {'a': '${foo}'}

## Nested config

Nested config is encoded using a flat mapping of dot-delimited names
to values. When decoded, nested config is represented by a deep dict
using dots to denote levels in the decoded dict.

### Decoding nested config

The decoding function is `gage._internal.util.nested_config`. It takes a flag
map of dot-delimited names to values.


    >>> def nc(kv, config=None):
    ...     from gage._internal.util import apply_nested_config
    ...     from pprint import pprint
    ...
    ...     config = config or {}
    ...     apply_nested_config(kv, config)
    ...     pprint(config)

    >>> nc({})
    {}

    >>> nc({"1": 1})
    {'1': 1}

    >>> nc({"1.1": 11})
    {'1': {'1': 11}}

    >>> nc({"1.1": 11, "1.2": 12})
    {'1': {'1': 11, '2': 12}}

Cannot nest within a non-dict:

    >>> nc({"1": 1, "1.1": 11, "1.2": 12})
    Traceback (most recent call last):
    ValueError: '1.1' cannot be nested: conflicts with {'1': 1}

    >>> nc({"1.2": 12, "1.1.1": 111, "1.2.1": 121})
    Traceback (most recent call last):
    ValueError: '1.2.1' cannot be nested: conflicts with {'1.2': 12}

An explicit dict is okay:

    >>> nc({"1.2": {}, "1.1.1": 111, "1.2.1": 121})
    {'1': {'1': {'1': 111}, '2': {'1': 121}}}

### Applying values to existing configuration

`apply_nested_config()` can be used to apply config to existing
values. If the specified data structure contains sections with
dot-names, Guild applies the config to the applicable sections without
creating nested dicts.

Simple case of no dot-names:

    >>> nc({"a": 1}, {"a": 2})
    {'a': 1}

    >>> nc({"a": 1}, {"b": 2})
    {'a': 1, 'b': 2}

Matching dot-names:

    >>> nc({"a.b": 1}, {"a.b": 2})
    {'a.b': 1}

    >>> nc({"a.b.c": 1}, {"a.b.c": 2})
    {'a.b.c': 1}

    >>> nc({"a.b.c.d": 11}, {"a.b.c.d": 22})
    {'a.b.c.d': 11}

Dot-name applied to various combinations of matches:

    >>> nc({"a.b": 1}, {"a": {}})
    {'a': {'b': 1}}

    >>> nc({"a.b": 1}, {"a": 2})
    Traceback (most recent call last):
    ValueError: 'a.b' cannot be nested: conflicts with {'a': 2}

    >>> nc({"a": 1}, {"a.b": 2})
    {'a': 1, 'a.b': 2}

    >>> nc({"a.b.c.d": 1}, {"a.b.c": {"d": 2}})
    {'a.b.c': {'d': 1}}

    >>> nc({"a.b.c.d": 1}, {"a.b": {"c.d": 2}})
    {'a.b': {'c.d': 1}}

    >>> nc({"a.b.c.d": 1}, {"a": {"b.c.d": 2}})
    {'a': {'b.c.d': 1}}

    >>> nc({"a.b.c.d": 1}, {"a": {"b.c": {"d": 2}}})
    {'a': {'b.c': {'d': 1}}}

    >>> nc({"a.b.c.d": 1}, {"a": {"b": {"c.d": 2}}})
    {'a': {'b': {'c.d': 1}}}

    >>> nc({"a.b.c.d": 1}, {"a": {"b": {"c": {"d": 2}}}})
    {'a': {'b': {'c': {'d': 1}}}}

Dot-name applied to empty matches:

    >>> nc({"a.b.c.d": 1}, {})
    {'a': {'b': {'c': {'d': 1}}}}

    >>> nc({"a.b.c.d": 1}, {"a": {}})
    {'a': {'b': {'c': {'d': 1}}}}

    >>> nc({"a.b.c.d": 1}, {"a": {"b": {}}})
    {'a': {'b': {'c': {'d': 1}}}}

    >>> nc({"a.b.c.d": 1}, {"a": {"b": {"c": {}}}})
    {'a': {'b': {'c': {'d': 1}}}}

    >>> nc({"a.b.c.d": 1}, {"a": {"b": {"c": {"d": {}}}}})
    {'a': {'b': {'c': {'d': 1}}}}

Dot-name applied to match and additional configuration:

    >>> nc({"a.b.c.d": 1}, {"a.b.c": {"d": 2, "e": 3}})
    {'a.b.c': {'d': 1, 'e': 3}}

Dot-name applied to both flat and nested matching config (highest
specified nested version is matched):

    >>> nc({"a.b.c.d": 1}, {"a.b.c.d": 2, "a.b.c": {"d": 3}})
    {'a.b.c': {'d': 3}, 'a.b.c.d': 1}

    >>> nc({"a.b.c.d": 1}, {"a.b": {"c.d": 2}, "a.b.c": {"d": 3}})
    {'a.b': {'c.d': 2}, 'a.b.c': {'d': 1}}

#### Name parts split

Tests for `_iter_dot_name_trials` - shows search order of dot names to
when applying key values to config in `nested_config()`.

    >>> def dot_name_trials(s):
    ...     from gage._internal.util import _iter_dot_name_trials
    ...     for trial in _iter_dot_name_trials(s):
    ...         print(trial)

    >>> dot_name_trials("a")
    a

    >>> dot_name_trials("a.b")
    a.b
    a

    >>> dot_name_trials("a.b.c")
    a.b.c
    a.b
    a

### Encoding nested config

The function `gage._internal.util.encode_nested_config` is used to encode a
deep dict to a flat dict with dot-delimited names.

    >>> from gage._internal.util import encode_nested_config as enc

    >>> enc({})
    {}

    >>> enc({"a": "A"})
    {'a': 'A'}

    >>> enc({"a.a1": "A1", "a.a2": "A2"})
    {'a.a1': 'A1', 'a.a2': 'A2'}

    >>> enc({"1": {"1": 11}})
    {'1.1': 11}

    >>> enc({"1": {"1": 11, "2": 12}})  # +pprint
    {'1.1': 11, '1.2': 12}

    >>> enc({"1": {"1": {"1": 111}, "2": {"1": 121}}})  # +pprint
    {'1.1.1': 111, '1.2.1': 121}

## Removing item from lists

The functions `safe_list_remove` and `safe_list_remove_all` are used
to safely remove items from lists.

    >>> from gage._internal.util import safe_list_remove
    >>> from gage._internal.util import safe_list_remove_all

Helper functions:

    >>> def rm(x, l):
    ...     safe_list_remove(x, l)
    ...     return l

    >>> def rm_all(xs, l):
    ...     safe_list_remove_all(xs, l)
    ...     return l

Examples:

    >>> rm(1, [1])
    []

    >>> rm(1, [])
    []

    >>> rm(1, [2])
    [2]

    >>> rm_all([1, 2], [2, 1])
    []

    >>> rm_all([1, 2], [])
    []

    >>> rm_all([1, 2], [2, 3])
    [3]

## Active shell

The active shell is provided as a shell string using
`util.active_shell()`.

    >>> from gage._internal.util import active_shell

    >>> shell = active_shell()

    >>> if shell is None:
    ...     import psutil
    ...     from gage._internal.util import shlex_quote as _quote, _KNOWN_SHELLS
    ...     proc_path = []
    ...     p = psutil.Process().parent()
    ...     while p:
    ...         proc_path.append(_quote(p.name()))
    ...         p = p.parent()
    ...     print(f"Unknown active shell (proc path: {' > '.join(reversed(proc_path))})")
    ...     print(f"Expected one of: {', '.join(_KNOWN_SHELLS)}")

### Active shell caching

The `util` module caches the active shell to avoid rereading the
environment. The cached value is `util._cached_active_shell`.

    >>> from gage._internal import util
    >>> assert util._cached_active_shell == shell

We can verify that the environment is read only when this value is set
to "__unset__". To test this, we temporarily replace the underlying
function that checks the environment (`util._active_shell`).

    >>> def _active_shell_replacement():
    ...     print("Reading the env for active shell")
    ...     return "xxx"  # value used as active shell

    >>> _active_shell_save = util._active_shell
    >>> shell_save = shell  # save value from above
    >>> util._active_shell = _active_shell_replacement

Reset the cached active shell and re-read the value.

    >>> util._cached_active_shell = "__unset__"
    >>> shell = util.active_shell()
    Reading the env for active shell

Call `active_shell()` again to verify that the environment is not read.

    >>> shell = util.active_shell()  # does not log 'Reading the env...'

    >>> shell
    'xxx'

Replace the environment read function.

    >>> util._active_shell = _active_shell_save

Force a re-read of the environment for active shell.

    >>> util._cached_active_shell = "__unset__"
    >>> shell = util.active_shell()

    >>> assert shell == shell_save

## Check Guild version

Guild `util` module provides a function to validate the current gage
ML version given a package requirement spec.

    >>> from gage._internal.util import check_gage_version
    >>> from gage import __version__ as current_gage_version

Test some version assertions that we know are true.

    >>> check_gage_version(">0.0.0")
    True

    >>> check_gage_version("<9999")
    True

Provide an invalid spec (error message varies across Python versions).

    >>> check_gage_version("not a valid spec")  # +wildcard
    Traceback (most recent call last):
    ValueError: Invalid version spec 'not a valid spec': ...

## Format duration

    >>> from gage._internal.util import format_duration as fd

    >>> fd(0, 0)
    '0:00:00'

    >>> fd(0, 60000000)
    '0:01:00'

    >>> fd(0, 600000000)
    '0:10:00'

    >>> fd(0, 3600000000)
    '1:00:00'

    >>> fd(0, 3601000000)
    '1:00:01'

    >>> fd(0, 3601000000.123)
    '1:00:01'

## Split lines

    >>> from gage._internal.util import split_lines

    >>> split_lines("")
    []

    >>> split_lines("a")
    ['a']

    >>> split_lines("ab")
    ['ab']

    >>> split_lines("a\nb")
    ['a', 'b']

    >>> split_lines("\nab")
    ['ab']

    >>> split_lines("ab\n")
    ['ab']

    >>> split_lines("a\r\nb")
    ['a', 'b']

    >>> split_lines("\r\nab")
    ['ab']

    >>> split_lines("ab\r\n")
    ['ab']

    >>> split_lines("a\n\nb")
    ['a', 'b']

    >>> split_lines("\n\nab")
    ['ab']

    >>> split_lines("ab\n\n")
    ['ab']

## Converting dict keys to camel case

Use `dict_to_camel_case` to convert pascal case dict keys to camel case. This
can be used when interfacing with JavaScript applications.

    >>> from gage._internal.util import dict_to_camel_case

    >>> dict_to_camel_case({})
    {}

    >>> dict_to_camel_case({"foo": 123, "bar": 456})  # +pprint
    {'bar': 456, 'foo': 123}

    >>> dict_to_camel_case({"foo_bar": 123, "bar_baz": 456})  # +pprint
    {'barBaz': 456, 'fooBar': 123}

    >>> dict_to_camel_case({"_foo": 123})
    {'_foo': 123}

    >>> dict_to_camel_case({"_foo_": 123})
    {'_foo_': 123}

    >>> dict_to_camel_case({"__foo__": 123})
    {'__foo__': 123}

    >>> dict_to_camel_case({"__foo_bar__": 123})
    {'__fooBar__': 123}

    >>> dict_to_camel_case({"__foo__bar__": 123})
    {'__foo_Bar__': 123}

    >>> dict_to_camel_case({"__foo___bar__": 123})
    {'__foo__Bar__': 123}

    >>> dict_to_camel_case({"_": 123})
    {'_': 123}

    >>> dict_to_camel_case({"__": 123})
    {'__': 123}

    >>> dict_to_camel_case({"": 123})
    {'': 123}

## Flatten list

    >>> from gage._internal.util import flatten

    >>> flatten([])
    []

    >>> flatten([[], []])
    []

    >>> flatten([[], [], []])
    []

    >>> flatten([[1]])
    [1]

    >>> flatten([[1, 2]])
    [1, 2]

    >>> flatten([[1, 2], [3, 4]])
    [1, 2, 3, 4]

    >>> flatten([[1, 2], [3, 4, 5], []])
    [1, 2, 3, 4, 5]

    >>> flatten([[1, 2], [3, 4, 5], [6]])
    [1, 2, 3, 4, 5, 6]

Only flattens to one level.

    >>> flatten([[1, 2], [3, 4, 5], [[6]]])
    [1, 2, 3, 4, 5, [6]]

## Encode/Decode CFG

CFG/INI decoding is provided by the function `decode_cfg_val`. This
function implements the behavior outlined in Python's `configparser`
support for [data types](https://docs.python.org/library/configparser.html#supported-datatypes).

    >>> from gage._internal.util import decode_cfg_val

Ints:

    >>> decode_cfg_val("0")
    0

    >>> decode_cfg_val("123")
    123

    >>> decode_cfg_val("012")
    12

Floats:

    >>> decode_cfg_val("1.23")
    1.23

Bools:

    >>> decode_cfg_val("yes")
    True

    >>> decode_cfg_val("no")
    False

    >>> decode_cfg_val("true")
    True

    >>> decode_cfg_val("false")
    False

    >>> decode_cfg_val("True")
    True

    >>> decode_cfg_val("False")
    False

Strings:

    >>> decode_cfg_val("something")
    'something'

    >>> decode_cfg_val("[something, 1, True, 1.7]")
    '[something, 1, True, 1.7]'

    >>> decode_cfg_val("0xdeadbeef")
    '0xdeadbeef'

CFG/INI encoding is simply the application of `str` to a value.

    >>> from gage._internal.util import encode_cfg_val

    >>> encode_cfg_val(123)
    '123'

    >>> encode_cfg_val(1.23)
    '1.23'

    >>> encode_cfg_val(True)
    'True'

    >>> encode_cfg_val(False)
    'False'

    >>> encode_cfg_val("something")
    'something'

    >>> encode_cfg_val([1, 1.23, True, "something"])
    "[1, 1.23, True, 'something']"

    >>> encode_cfg_val({"a": 1})
    "{'a': 1}"

## `any_apply` and `all_apply`

These two functions evaluate a series of functions and return True or
False depending on the function and the truthiness of the applied
results.

    >>> from gage._internal.util import any_apply, all_apply

Create functions that evaluate True and False, printing that they're
called.

    >>> def t():
    ...     print('t()')
    ...     return True

    >>> def f():
    ...     print('f()')
    ...     return False

`any_apply` returns early with True on the first True, otherwise
returns False.

    >>> any_apply([])
    False

    >>> any_apply([t])
    t()
    True

    >>> any_apply([f])
    f()
    False

    >>> any_apply([f, t])
    f()
    t()
    True

    >>> any_apply([t, f])
    t()
    True

    >>> any_apply([f, f])
    f()
    f()
    False

    >>> any_apply([t, t])
    t()
    True

`any_all` returns early with False on the first False, otherwise
returns True.

    >>> all_apply([])
    True

    >>> all_apply([t])
    t()
    True

    >>> all_apply([f])
    f()
    False

    >>> all_apply([f, t])
    f()
    False

    >>> all_apply([t, f])
    t()
    f()
    False

    >>> all_apply([f, f])
    f()
    False

    >>> all_apply([t, t])
    t()
    t()
    True

## Pop find

`util.pop_find` removes and returns the first element in a list
matching a filter call.

    >>> from gage._internal.util import pop_find

    >>> l = [1,2,3]
    >>> pop_find(l, lambda x: x >=2)
    2

    >>> l
    [1, 3]

If the filter does not match an element, returns a default value
without modifying the list. If not specified the default is None.

    >>> print(pop_find(l, lambda x: False))
    None

    >>> l
    [1, 3]

    >>> pop_find(l, lambda x: False, 'another default')
    'another default'

    >>> l
    [1, 3]

## Apply acc

`apply_acc()` appends non-None function results to a list.


    >>> from gage._internal.util import apply_acc

    >>> apply_acc([
    ...     lambda: 1,
    ...     lambda: 2,
    ...     lambda: None,
    ...     lambda: 3,
    ...     lambda: None])
    [1, 2, 3]
