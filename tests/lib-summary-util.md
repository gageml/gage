# Command Impl Support

## Format Summary Values

    >>> from gage._internal.summary_util import format_summary_value

    >>> format_summary_value(1)
    '1'

    >>> format_summary_value(1.0)
    '1'

    >>> format_summary_value(1.123)
    '1.123'

    >>> format_summary_value(1.123456789)
    '1.123'

    >>> format_summary_value(-1.123e-3)
    '-0.001123'

    >>> format_summary_value(-1.123456e-9)
    '-1.123e-09'

    >>> format_summary_value(True)
    'true'

    >>> format_summary_value(False)
    'false'

    >>> format_summary_value("hello")
    'hello'

    >>> format_summary_value(None)
    ''

    >>> format_summary_value({"value": 123.4})
    '123.4'

    >>> format_summary_value({"value": 123.456789, "color": "green"})
    '123.5'

    >>> format_summary_value({"foo": 123.4})
    ''

    >>> format_summary_value([1, 2, 3])
    '1, 2, 3'

    >>> format_summary_value([True, {"value": 0.12345}, None])
    'true, 0.1235, '
