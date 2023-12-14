# Op ref utils

`opref_util` provides functionality related to op references.

An op reference ("op ref") is a short representation of an operation.

    >>> from gage._internal.types import OpRef

An op ref consists of a namespace and a name.

    >>> test_op = OpRef("test", "test")

## Encoding op refs

Op refs can be encoded to a string using `encode_opref`.

    >>> from gage._internal.opref_util import encode_opref

An encoded op ref consists of a schema followed by a space followed by
the encoded op ref. The current schema is `1`. The encoding is a
space-delimited string of namespace and name.

    >>> encode_opref(OpRef("test", "test"))
    '1 test test'

Both namespace and name are required.

    >>> encode_opref(OpRef("", ""))
    Traceback (most recent call last):
    ValueError: opref namespace (op_ns) cannot be empty

    >>> encode_opref(OpRef("test", ""))
    Traceback (most recent call last):
    ValueError: opref name (op_name) cannot be empty

Neither namespace nor name may contain spaces.

    >>> encode_opref(OpRef("foo bar", "test"))
    Traceback (most recent call last):
    ValueError: opref namespace (op_ns) cannot contain spaces

    >>> encode_opref(OpRef("foo_bar", "a test"))
    Traceback (most recent call last):
    ValueError: opref name (op_name) cannot contain spaces
