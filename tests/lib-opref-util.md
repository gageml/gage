# Op Ref Utils

`opref_util` provides functionality related to op references.

An op reference ("op ref") is a short representation of an operation.

    >>> from gage._internal.types import OpRef

An op ref consists of a namespace and a name.

    >>> test_op = OpRef("test", "test")

## Encoding Op Refs

Op refs can be encoded to a string using `encode_opref`.

    >>> from gage._internal.opref_util import encode_opref

An encoded op ref consists of a schema followed by a space followed by
the encoded op ref. The current schema is `2`. The encoding is a
space-delimited string of namespace, name, and version.

    >>> encode_opref(OpRef("test", "test", "1"))
    '2 test test 1'

Both namespace and name are required.

    >>> encode_opref(OpRef("", ""))
    Traceback (most recent call last):
    ValueError: opref namespace (op_ns) cannot be empty

    >>> encode_opref(OpRef("test", ""))
    Traceback (most recent call last):
    ValueError: opref name (op_name) cannot be empty

None of the arguments can contain spaces.

    >>> encode_opref(OpRef("foo bar", "test"))
    Traceback (most recent call last):
    ValueError: opref namespace (op_ns) cannot contain spaces

    >>> encode_opref(OpRef("foo_bar", "a test"))
    Traceback (most recent call last):
    ValueError: opref name (op_name) cannot contain spaces

    >>> encode_opref(OpRef("foo_bar", "test", "a version"))
    Traceback (most recent call last):
    ValueError: opref version (op_version) cannot contain spaces

## Decoding Op Refs

Op refs are decoded from strings using `decode_opref`.

    >>> from gage._internal.opref_util import decode_opref

The current encoding schema is '2', which supports namespace, name, and
version.

    >>> decode_opref("2 my-ns my-op 1")
    <OpRef ns="my-ns" name="my-op" version="1">

Version may be omitted.

    >>> decode_opref("2 my-ns my-op")
    <OpRef ns="my-ns" name="my-op">

Both namespace and op name are required.

    >>> decode_opref("2 my-ns")
    Traceback (most recent call last):
    ValueError: invalid opref encoding: '2 my-ns'

    >>> decode_opref("2")
    Traceback (most recent call last):
    ValueError: invalid opref encoding: '2'

### v1 Schema

Gage supports version 1 schema.

    >>> decode_opref("1 my-ns my-op")
    <OpRef ns="my-ns" name="my-op">

Additional parts are not supported (e.g. version).

    >>> decode_opref("1 my-ns my-op 1")
    Traceback (most recent call last):
    ValueError: invalid opref encoding: '1 my-ns my-op 1'

### Other Schema Versions

Other schema versions aren't supported.

    >>> decode_opref("3 foobar")
    Traceback (most recent call last):
    ValueError: unsupported opref schema: '3 foobar'
