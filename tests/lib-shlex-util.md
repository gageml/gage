# `shlex_util`

`shlex_util` augments Python's `shlex` module functionality and should
be used as a replacement in all cases.

    >>> from gage._internal.shlex_util import *

## Split

    >>> shlex_split(None)
    []

    >>> shlex_split("")
    []

    >>> shlex_split("foo")
    ['foo']

    >>> shlex_split("foo bar")
    ['foo', 'bar']

    >>> shlex_split("'foo bar'")
    ['foo bar']

    >>> shlex_split("'foo bar' baz")
    ['foo bar', 'baz']

    >>> shlex_split("'/foo/bar'")
    ['/foo/bar']

    >>> shlex_split("'/foo bar'")
    ['/foo bar']

    >>> shlex_split("'/foo bar' baz bam")
    ['/foo bar', 'baz', 'bam']

    >>> shlex_split("'\\foo\\bar'")
    ['\\foo\\bar']

    >>> shlex_split("a b c")
    ['a', 'b', 'c']

    >>> shlex_split("a \"b c\"")
    ['a', 'b c']

    >>> shlex_split("'a b' c")
    ['a b', 'c']

## Quote

    >>> shlex_quote(None)
    "''"

    >>> shlex_quote("")
    "''"

    >>> shlex_quote("foo")
    'foo'

    >>> shlex_quote("foo bar")
    "'foo bar'"

    >>> shlex_quote("/foo/bar")
    '/foo/bar'

    >>> shlex_quote("/foo bar")
    "'/foo bar'"

    >>> shlex_quote("\\foo\\bar")
    "'\\foo\\bar'"

    >>> shlex_quote("D:\\foo\\bar")
    "'D:\\foo\\bar'"

    >>> shlex_quote("D:\\foo bar")
    "'D:\\foo bar'"

    >>> shlex_quote("'a b c'")
    '"\'a b c\'"'

    >>> shlex_quote("~")
    "'~'"

    >>> shlex_quote("a ~ b")
    "'a ~ b'"

    >>> shlex_quote("*")
    "'*'"

    >>> shlex_quote("?")
    "'?'"

    >>> shlex_quote("a b")
    "'a b'"

    >>> shlex_quote("\"a\" b")
    '\'"a" b\''

    >>> shlex_quote("'a' b")
    '\'\'"\'"\'a\'"\'"\' b\''

## Join

    >>> shlex_join(["a", "b c"])
    "a 'b c'"
