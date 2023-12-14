# Language support

The `lang` module provides Gage specific language parsers.

    >>> from gage._internal.lang import *

## Run config values

Run config values are provided via flag assignments to the `run`
command. String values are parsed with `parse_config_value()`.

    >>> parse_config_value("1")
    1

    >>> parse_config_value("+1")
    1

    >>> parse_config_value("-1")
    -1

    >>> parse_config_value("1.1")
    1.1

    >>> parse_config_value("+1.1")
    1.1

    >>> parse_config_value("-1.1")
    -1.1

    >>> parse_config_value("1e0")
    1.0

    >>> parse_config_value("+1e0")
    1.0

    >>> parse_config_value("-1e0")
    -1.0

    >>> parse_config_value("1.11e1")
    11.1

    >>> parse_config_value("+1.11e1")
    11.1

    >>> parse_config_value("-1.11e1")
    -11.1

    >>> parse_config_value("true")
    True

    >>> parse_config_value("false")
    False

    >>> parse_config_value("null")  # +pprint
    None

    >>> parse_config_value("'1'")
    '1'

    >>> parse_config_value("\"1\"")
    '1'

    >>> parse_config_value("hello")
    'hello'

    >>> parse_config_value("False")
    'False'

    >>> parse_config_value("[1,2,3]")
    '[1,2,3]'

    >>> parse_config_value("a bunch of words")
    'a bunch of words'

    >>> parse_config_value("  ")
    ''

    >>> parse_config_value("")
    ''

## Flag assigns

    >>> parse_flag_assign("")
    Traceback (most recent call last):
    ValueError: Missing expected token: one of KEY, QUOTED_STRING

    >>> parse_flag_assign("a=1")
    ('a', 1)

    >>> parse_flag_assign("a=-1.123")
    ('a', -1.123)

    >>> parse_flag_assign("a=")
    ('a', '')

    >>> parse_flag_assign("aa=123")
    ('aa', 123)

    >>> parse_flag_assign("aa = 123")
    ('aa', 123)

    >>> parse_flag_assign("a b = 1 2 3")
    Traceback (most recent call last):
    ValueError: Unexpected token: 'b'

    >>> parse_flag_assign("'a b' = '1 2 3'")
    ('a b', '1 2 3')
