# `gage.cli`

The `cli` library provides an interface for interacting with the user
over a command line interface.

    >>> from gage._internal import cli

    >>> cli.out("Hello")
    Hello

    >>> with StderrCapture() as err:
    ...     cli.err("Bye, sad")

    >>> err.print()
    Bye, sad

## Format Assignment Lists

The function `format_assigns` returns a formatted string for a list of
name value pairs.

    >>> format = cli.format_assigns

    >>> format([], 10)
    ''

    >>> format([("foo", "123")], 10)
    'foo=123'

    >>> format([("foo", "123"), ("bar", "456")], 10)
    'foo=123'

    >>> format([("bar", "456"), ("foo", "123")], 10)
    'bar=456'

    >>> format([("foo", "123456")], 10)
    'foo=123456'

    >>> format([("foo", "1234567")], 10)
    'foo=12345…'

    >>> format([("foobar", "1234567")], 10)
    'foob…=123…'

    >>> format([("foobar", "1234567")], 13)
    'foobar=12345…'

    >>> format([("foobar", "1234567")], 14)
    'foobar=1234567'

    >>> format([("foobar", "1234567"), ("barfoo", "7654321")], 29)
    'foobar=1234567 barfoo=7654321'

    >>> format([("foobar", "1234567"), ("barfoo", "7654321")], 28)
    'foobar=1234567 barfoo=76543…'

This is the smallest window to show both assignments. Any less available
space would violate the minimum assignment width of 11.

    >>> format([("foobar", "1234567"), ("barfoo", "7654321")], 23)
    'foob…=1234… barf…=7654…'

When we reduce the available width, the formatted result is truncated to
a single item.

    >>> format([("foobar", "1234567"), ("barfoo", "7654321")], 22)
    'foobar=1234567'

We can change the minimum assign width to accommodate both assignments
within this width.

    >>> format(
    ...     [("foobar", "1234567"), ("barfoo", "7654321")], 22,
    ...     min_assign_width=10
    ... )
    'foob…=1234… barf…=765…'

Format other examples.

    >>> format([
    ...     ("foobar", "1234567"),
    ...     ("barfoo", "7654321"),
    ...     ("baz", "321"),
    ... ], 31)
    'foob…=1234… barf…=7654… baz=321'

    >>> format([
    ...     ("foobar", "12345678"),
    ...     ("barfoo", "7654321"),
    ...     ("baz", "321"),
    ... ], 28)
    'foobar=123456… barfoo=76543…'

    >>> format([
    ...     ("foobar", "1234567"),
    ...     ("barfoo", "7654321"),
    ...     ("baz", "321"),
    ... ], 36)
    'foobar=1234567 barfoo=76543… baz=321'

    >>> format([
    ...     ("foobar", "1234567"),
    ...     ("barfoo", "7654321"),
    ...     ("baz", "321"),
    ...     ("bam", "45678"),
    ... ], 36)
    'foobar=1234567 barfoo=76543… baz=321'

### Implementation

`format_assign` is implementing using two phases:

- Truncate the list of assigns to maintain a minimum assign width
- Truncate the name and value for an assign to maintain a budget

#### `_fit_assigns`

Truncate assigns is implemented by `_fit_assigns`.

    >>> fit_assigns = cli._fit_assigns

    >>> fit_assigns([], 10)
    []

`_fit_assigns` always returns at least one assign when assigns is not
empty.

    >>> fit_assigns([("foo", "123")], 0)
    [('foo', '123')]

`_fit_assigns` returns as many assignments as it can fit given three
parameters: total width, min assign width, and max assign width.

Min assign width is 11. This provides width for at least 4 non-truncated
chars for name and value and an equal signs.

    >>> cli.MIN_ASSIGN_WIDTH
    11

`_fit_assigns` preserves the order of items. It always returns at least
one item regardless of max width.

The following two scenarios only allow for the first assign (widths 0 through 11).

    >>> fit_assigns([("xxxxxx", "X"), ("a", "A"), ("b", "BB")], 0)
    [('xxxxxx', 'X')]

    >>> fit_assigns([("xxxxxx", "X"), ("a", "A"), ("b", "BB")], 11)
    [('xxxxxx', 'X')]

Widths of 12 through 16 fit two assigns fit.

    >>> fit_assigns([("xxxxxx", "X"), ("a", "A"), ("b", "BB")], 12)
    [('xxxxxx', 'X'), ('a', 'A')]

    >>> fit_assigns([("xxxxxx", "X"), ("a", "A"), ("b", "BB")], 16)
    [('xxxxxx', 'X'), ('a', 'A')]

Width of 17 fits all assigns.

    >>> fit_assigns([("xxxxxx", "X"), ("a", "A"), ("b", "BB")], 17)
    [('xxxxxx', 'X'), ('a', 'A'), ('b', 'BB')]

#### `_fit_assign`

Truncating an assign is implemented by `_fit_assign`.

    >>> fit_assign = cli._fit_assign

`_fit_assign` truncated both name and value (a "part") according to the
relative size of these values when compared to the total assign width.
When a part is truncated, it ends with an ellipsis (…) char.

An assign uses at least three chars.

    >>> fit_assign("a", "b", 2)
    ('a', 'b')

By default, names are preserved up to the first four chars.

    >>> fit_assign("abcd", "", 0)
    ('abcd', '…')

Note that a five char name is not truncated as the ellipsis requires one
char and so does not shorten the name,

    >>> fit_assign("abcde", "", 0)
    ('abcde', '…')

Names are truncated at six chars.

    >>> fit_assign("abcdef", "", 0)
    ('abcd…', '…')

The min name width is configurable with the `min_name` param.

    >>> fit_assign("abcde", "", 0, min_name=3)
    ('abc…', '…')

Values are truncated with consideration for an additional assignment
char (i.e. "="). For example, a budget of 8 will produce a total name
and value length of 7.

    >>> fit_assign("abcdef", "123456", 8)
    ('abcd…', '1…')

And so on.

    >>> fit_assign("abcdef", "123456", 9)
    ('abcd…', '12…')

    >>> fit_assign("abcdef", "123456", 10)
    ('abcd…', '123…')

When name and value are the same size, they're truncated equally.

    >>> fit_assign("abcdef", "123456", 11)
    ('abcd…', '1234…')

Name and value are truncated based on their relative widths. For
example, if value is 2x the width of name, it's truncated by 2x the
chars as needed fit within the budget.

    >>> fit_assign("abcdef", "123456789012", 13)
    ('abcd…', '123456…')
