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

    >>> shlex_quote(None)  # -windows
    "''"

    >>> shlex_quote(None)  # +windows
    '""'

    >>> shlex_quote("")  # -windows
    "''"

    >>> shlex_quote("")  # +windows
    '""'

    >>> shlex_quote("foo")
    'foo'

    >>> shlex_quote("foo bar")  # -windows
    "'foo bar'"

    >>> shlex_quote("foo bar") # +windows
    '"foo bar"'

    >>> shlex_quote("/foo/bar")
    '/foo/bar'

    >>> shlex_quote("/foo bar")  # -windows
    "'/foo bar'"

    >>> shlex_quote("/foo bar")  # +windows
    '"/foo bar"'

    >>> shlex_quote("\\foo\\bar")  # -windows
    "'\\foo\\bar'"

    >>> shlex_quote("\\foo\\bar")  # +windows
    '\\foo\\bar'

    >>> shlex_quote("D:\\foo\\bar")  # -windows
    "'D:\\foo\\bar'"

    >>> shlex_quote("D:\\foo\\bar")  # +windows
    'D:\\foo\\bar'

    >>> shlex_quote("D:\\foo bar")  # -windows
    "'D:\\foo bar'"

    >>> shlex_quote("D:\\foo bar")  # +windows
    '"D:\\foo bar"'

    >>> shlex_quote("'a b c'")
    '"\'a b c\'"'

    >>> shlex_quote("~")  # -windows
    "'~'"

    >>> shlex_quote("~")  # +windows
    '"~"'

    >>> shlex_quote("a ~ b")  # -windows
    "'a ~ b'"

    >>> shlex_quote("a ~ b")  # +windows
    '"a ~ b"'

    >>> shlex_quote("*")  # -windows
    "'*'"

    >>> shlex_quote("*")  # +windows
    '"*"'

    >>> shlex_quote("?")  # -windows
    "'?'"

    >>> shlex_quote("?")  # +windows
    '"?"'

    >>> shlex_quote("a b")  # -windows
    "'a b'"

    >>> shlex_quote("a b")  # +windows
    '"a b"'

    >>> shlex_quote("\"a\" b")  # -windows
    '\'"a" b\''

    >>> shlex_quote("\"a\" b")  # +windows
    '"\\"a\\" b"'

    >>> shlex_quote("'a' b")  # -windows
    '\'\'"\'"\'a\'"\'"\' b\''

    >>> shlex_quote("'a' b")  # +windows
    '"\'a\' b"'

## Join

    >>> shlex_join(["a", "b c"])  # -windows
    "a 'b c'"

    >>> shlex_join(["a", "b c"])  # +windows
    'a "b c"'

## Unquoting

    >>> from gage._internal.shlex_util import _unquote

    >>> _unquote("'Hello'")
    'Hello'

    >>> _unquote("")
    ''

    >>> _unquote("''")
    ''

    >>> _unquote("Hello")
    'Hello'

    >>> _unquote("'Hello")
    "'Hello"

## Splitting Env

`split_env` removes env assignments from a string, returning them in an
env dict.

    >>> from gage._internal.shlex_util import split_env

    >>> split_env("")
    ('', {})

    >>> split_env("python eval.py")
    ('python eval.py', {})

    >>> split_env("FOO=123 BAR=abc BAZ=\"Hello there\" BAM='Hi ho' python eval.py")
    ... # -space
    ('python eval.py',
     {'FOO': '123',
      'BAR': 'abc',
      'BAZ': 'Hello there',
      'BAM': 'Hi ho'})
