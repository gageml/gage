# Summary Samples

A summary is a file named `summary.py` located in a run meta directory.

Samples are provided in the `summary` samples directory.

    >>> cd(sample("summary"))

Create a function to apply the JSON Schema `summary.schema.json` to
each sample.

    >>> def validate(filename):
    ...     from gage._internal import schema_util
    ...     data = json.load(open(filename))
    ...     result = schema_util.validate_data(data, "summary")
    ...     if not result.valid:
    ...         print(json.dumps(
    ...            result.output("basic")["errors"],
    ...            indent=2, sort_keys=True)
    ...         )


Empty summaries are allowed.

    >>> validate("empty.json")

A simple summary with attributes and metrics.

    >>> validate("simple.json")

Attributes and metrics can have object values, which must specify a
`value`.

    >>> validate("object-values.json")

When an object is specified, `value` is required.

    >>> validate("missing-value.json")  # +parse -space
    {}
    "error": "The object is missing required properties ['value']",
    "instanceLocation": "/metrics/x",
    {}

Attribute values must be numbers, strings, booleans, or null.

    >>> validate("invalid-attribute-value.json")  # +parse -space
    {}
    "error": "The instance must be of type
    [\"string\", \"number\", \"boolean\", \"null\"]",
    "instanceLocation": "/attributes/a/value",
    {}

Metric values must be numbers of null.

    >>> validate("invalid-metric-value.json")  # +parse -space
    {}
    "error": "The instance must be of type [\"number\", \"null\"]",
    "instanceLocation": "/metrics/x",
    {}

Attributes and metrics can provide additional attributes as metadata.

    >>> validate("metadata.json")

Attributes and metric value object can have a `label` attribute, which
is used when displaying the value (e.g. for a board column). When
specified, labels must be strings.

    >>> validate("invalid-attribute-label.json")  # +parse -space
    {}
    "error": "The instance must be of type \"string\"",
    "instanceLocation": "/attributes/a/label",
    {}

    >>> validate("invalid-metric-label.json")  # +parse -space
    {}
    "error": "The instance must be of type \"string\"",
    "instanceLocation": "/metrics/x/label",
    {}
