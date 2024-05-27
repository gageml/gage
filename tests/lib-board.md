# Board Def Support

The `board` module is responsible for validating and loading board
config.

    >>> from gage._internal.board import *

Other required imports:

    >>> from gage._internal.types import *
    >>> from gage._internal import schema_util

    >>> def validate(data):
    ...     try:
    ...         validate_board_data(data)
    ...     except BoardConfigValidationError as e:
    ...         for error in schema_util.validation_errors(e):
    ...             print(error)
    ...     else:
    ...         print("ok")

## Empty Config

    >>> validate({})
    ok

## Invalid Top-Level Types

    >>> validate(123)
    The instance must be of type "object"

## Unknown Top-Level Properties

Invalid top-level attribute:

    >>> validate({"foo": 123})
    Additional properties: ['foo']

## Core Properties

    >>> validate({
    ...     "id": "abc123",
    ...     "name": "my-board",
    ...     "title": "My Board",
    ...     "description": "Just a sample board.",
    ... })
    ok

## Columns

    >>> def validate_col(data):
    ...     validate({"columns": [data]})

### Column Field

Column fields are specified using either a `field` attribute or one of
the supported field types (`run-attr`, `metric`, `attribute`, `config`).

A field spec is required.

    >>> validate_col({})  # +diff -space
    Properties ['columns'] are invalid
    Items: [0]
    The instance must be valid against exactly one subschema;
      it is valid against [] and invalid against [0, 1]
    The instance must be valid against exactly one subschema;
      it is valid against [] and invalid against [0, 1, 2, 3, 4]
    The object is missing required properties ['field']
    The object is missing required properties ['run-attr']
    The object is missing required properties ['metric']
    The object is missing required properties ['attribute']
    The object is missing required properties ['config']
    The instance must be of type "string"
    The instance must be valid against exactly one subschema;
      it is valid against [0, 1, 2, 3] and invalid against []

When using `field`, the value must start with a valid field type prefix
(i.e. `run-attr`, `metric`, `attribute`, `config`).

    >>> validate_col({"field": "foo"})  # +wildcard
    Properties ['columns'] are invalid
    ...
    The text must match the regular expression "^run-attr:.+$"
    The text must match the regular expression "^metric:.+$"
    The text must match the regular expression "^attribute:.+$"
    The text must match the regular expression "^config:.+$"
    ...

    >>> validate_col({"field": "run-attr:"})  # +wildcard
    Properties ['columns'] are invalid
    ...
    The text must match the regular expression "^run-attr:.+$"
    The text must match the regular expression "^metric:.+$"
    The text must match the regular expression "^attribute:.+$"
    The text must match the regular expression "^config:.+$"
    ...

    >>> validate_col({"field": "run-attr:id"})
    ok

    >>> validate_col({"field": "metric:foo"})
    ok

    >>> validate_col({"field": "attribute:foo"})
    ok

    >>> validate_col({"field": "config:foo"})
    ok

A field may alternatively be specified as a string.

    >>> validate_col("run-attr:name")
    ok

    >>> validate_col("invalid-field-ref")  # +wildcard
    Properties ['columns'] are invalid
    ...
    The text must match the regular expression "^run-attr:.+$"
    The text must match the regular expression "^metric:.+$"
    The text must match the regular expression "^attribute:.+$"
    The text must match the regular expression "^config:.+$"

Fields may be specified using properties that match the field type.

    >>> validate_col({"run-attr": "id"})
    ok

    >>> validate_col({"metric": "loss"})
    ok

    >>> validate_col({"attribute": "type"})
    ok

    >>> validate_col({"config": "lr"})
    ok

### Column Label

    >>> validate_col({"metric": "train_loss", "label": "Training Loss"})
    ok

    >>> validate_col({"metric": "train_loss", "label": 123})  # +wildcard
    Properties ['columns'] are invalid
    ...
    Properties ['label'] are invalid
    The instance must be of type "string"
    ...

### Column Description

    >>> validate_col({"metric": "loss", "description": "Final training loss"})
    ok

    >>> validate_col({"metric": "train_loss", "description": 123})  # +wildcard
    Properties ['columns'] are invalid
    ...
    Properties ['description'] are invalid
    The instance must be of type "string"
    ...

### Column Data Type

    >>> validate_col({"metric": "loss", "type": "currency"})
    ok

    >>> validate_col({"metric": "train_loss", "type": 123})  # +wildcard
    Properties ['columns'] are invalid
    ...
    Properties ['type'] are invalid
    The instance must be of type "string"
    ...

### Column Links

    >>> validate_col({"metric": "loss", "links": [
    ...     {"href": "https://foo.com/1"},
    ...     {"href": "https://foo.com/1", "label": "Foo 1"},
    ...     {"href": "https://foo.com/1", "type": "website"},
    ... ]})
    ok

    >>> validate_col({"metric": "loss", "links": [
    ...     {"href": "https://foo.com/1", "type": "not-supported"},
    ... ]})  # +wildcard -space
    Properties ['columns'] are invalid
    ...
    Properties ['type'] are invalid
    Value must be one of: ['paper', 'model', 'dataset', 'leaderboard',
                           'code', 'help', 'website']
    ...

### Column Hidden State

    >>> validate_col({"metric": "loss", "hide": True})
    ok

    >>> validate_col({"metric": "loss", "hide": False})
    ok

    >>> validate_col({"metric": "loss", "hide": "no"})  # +wildcard
    Properties ['columns'] are invalid
    ...
    Properties ['hide'] are invalid
    The instance must be of type "boolean"
    ...

### Column Sort Direction

    >>> validate_col({"metric": "loss", "sort": "asc"})
    ok

    >>> validate_col({"metric": "loss", "sort": "desc"})
    ok

    >>> validate_col({"metric": "loss", "sort": "other"})  # +wildcard
    Properties ['columns'] are invalid
    ...
    Properties ['sort'] are invalid
    Value must be one of: ['asc', 'desc']
    ...

### Column Filter

Columns filter spec can be a string or a boolean.

    >>> validate_col({"config": "x", "filter": "text"})
    ok

    >>> validate_col({"config": "x", "filter": True})
    ok

    >>> validate_col({"config": "x", "filter": False})
    ok

    >>> validate_col({"config": "x", "filter": {}})  # +wildcard -space
    Properties ['columns'] are invalid
    ...
    Properties ['filter'] are invalid
    The instance must be valid against exactly one subschema;
      it is valid against [] and invalid against [0, 1]
    The instance must be of type "string"
    The instance must be of type "boolean"
    ...

Other filter settings:

    >>> validate_col({"config": "x", "flag-filter-label": "Xxx"})
    ok

    >>> validate_col({"config": "x", "filter-select-all": False})
    ok

    >>> validate_col({"config": "x", "filter-search": False})
    ok

### Column Aggregation

    >>> validate_col({"metric": "loss", "agg": False})
    ok

    >>> validate_col({"metric": "total", "agg": "sum"})
    ok

    >>> validate_col({"metric": "loss", "agg": "min"})
    ok

    >>> validate_col({"metric": "loss", "agg": "other"})  # +wildcard -space
    Properties ['columns'] are invalid
    ...
    Value must be one of: [False, 'sum', 'min', 'max', 'count',
                           'avg', 'first', 'last']
    ...

Aggregation is true by default.

    >>> validate_col({"metric": "loss", "agg": True})  # +wildcard -space
    Properties ['columns'] are invalid
    ...
    Value must be one of: [False, 'sum', 'min', 'max', 'count',
                           'avg', 'first', 'last']
    ...

### Column Pinning

    >>> validate_col({"run-attr": "id", "pinned": "left"})
    ok

    >>> validate_col({"run-attr": "id", "pinned": "right"})
    ok

    >>> validate_col({"run-attr": "id", "pinned": "other"})  # +wildcard
    Properties ['columns'] are invalid
    ...
    Properties ['pinned'] are invalid
    Value must be one of: ['left', 'right']
    ...

### Row Groups

    >>> validate_col({"config": "model", "row-group": True})
    ok

    >>> validate_col({"config": "model", "row-group": 123})  # +wildcard
    Properties ['columns'] are invalid
    ...
    Properties ['row-group'] are invalid
    The instance must be of type "boolean"
    ...

### Unknown Column Properties

Columns can't have properties that aren't explicitly supported.

    >>> validate_col({"metric": "foo", "some-other": "123"})  # -space +diff
    Properties ['columns'] are invalid
    Items: [0]
    The instance must be valid against exactly one subschema;
      it is valid against [] and invalid against [0, 1]
    Additional properties: ['some-other']
    The instance must be of type "string"
    The instance must be valid against exactly one subschema;
      it is valid against [0, 1, 2, 3] and invalid against []

## Group Column

The `group-column` attribute defines the group column attributes.

    >>> validate({
    ...     "group-column": {
    ...         "label": "Custom Group"
    ...     }
    ... })
    ok

## Run Select

`run-select` is used to select runs for the board.

Runs can be selected on the basis of their operation name and status.

    >>> validate({
    ...     "run-select": {
    ...         "status": "completed",
    ...         "operation": "eval"
    ...     }
    ... })
    ok

Status can be an array.

    >>> validate({
    ...     "run-select": {
    ...         "status": ["completed", "terminated"]
    ...     }
    ... })
    ok

Status must be one of `completed`, `terminated`, `error`, or `running`.

    >>> validate({
    ...     "run-select": {
    ...         "status": "other"
    ...     }
    ... })  # -space
    Properties ['run-select'] are invalid
    Properties ['status'] are invalid
    The instance must be valid against exactly one subschema;
      it is valid against [] and invalid against [0, 1]
    Value must be one of: ['completed', 'terminated', 'error', 'running']
    The instance must be of type "array"

    >>> validate({
    ...     "run-select": {
    ...         "status": ["completed", "other"]
    ...     }
    ... })  # -space
    Properties ['run-select'] are invalid
    Properties ['status'] are invalid
    The instance must be valid against exactly one subschema;
      it is valid against [] and invalid against [0, 1]
    Value must be one of: ['completed', 'terminated', 'error', 'running']
    Items: [1]
    Value must be one of: ['completed', 'terminated', 'error', 'running']

    >>> validate({
    ...     "run-select": {
    ...         "status": 123
    ...     }
    ... })  # -space
    Properties ['run-select'] are invalid
    Properties ['status'] are invalid
    The instance must be valid against exactly one subschema;
      it is valid against [] and invalid against [0, 1]
    Value must be one of: ['completed', 'terminated', 'error', 'running']
    The instance must be of type "array"

Operation must be a string.

    >>> validate({
    ...     "run-select": {
    ...         "operation": 123
    ...     }
    ... })
    Properties ['run-select'] are invalid
    Properties ['operation'] are invalid
    The instance must be of type "string"

Additional properties aren't supported.

    >>> validate({
    ...     "run-select": {
    ...         "other": 123
    ...     }
    ... })
    Properties ['run-select'] are invalid
    Additional properties: ['other']
