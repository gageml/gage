# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import json
import os

# Avoid jschon import here - expensive

__all__ = [
    "validate_data",
    "validation_error_output",
    "validation_errors",
]

__schema: dict[str, Any] = {}


class ValidationError:

    def __init__(self, error_data: dict[str, Any], schema: dict[str, Any]):
        self.error_data = error_data
        self.schema = schema
        self._msg = _validation_error_msg(error_data, schema)

    def __str__(self):
        return self._msg


def validate_data(obj: JSONCompatible, schema_name: str):
    import jschon

    schema = _ensure_schema(schema_name)
    return schema.evaluate(jschon.JSON(obj))


def _ensure_schema(name: str):
    import jschon

    schema = __schema.get(name)
    if not __schema:
        catalog = jschon.create_catalog("2020-12")
        __schema[name] = schema = _load_schema(name)
    assert schema
    return cast(jschon.JSONSchema, schema)


def _load_schema(name: str):
    import jschon

    src = os.path.join(
        os.path.dirname(os.path.dirname(__file__)),
        f"{name}.schema.json",
    )
    with open(src) as f:
        schema_data = json.load(f)
    return jschon.JSONSchema(schema_data)


def validation_error_output(e: SchemaValidationError, verbose: bool = False):
    return e.validation_result.output("verbose" if verbose else "basic")


def validation_errors(e: SchemaValidationError):
    return [
        ValidationError(error_data, e.validation_result.schema)
        for error_data in (e.validation_result.output("basic").get("errors") or [])
    ]


def _validation_error_msg(data: dict[str, Any], schema: dict[str, Any]):
    kwLocation = data["keywordLocation"]
    error = data["error"]
    if kwLocation.endswith("/additionalProperties"):
        return f"Additional properties: {error}"
    elif kwLocation.endswith("/items"):
        return f"Items: {error}"
    elif kwLocation.endswith("/enum"):
        return f"Value must be one of: {_enum_value(kwLocation, schema)}"
    else:
        return str(error)


def _enum_value(location: str, schema: dict[str, Any]):
    assert location[:1] == "/", location
    path = location[1:].split("/")
    assert path[-1] == "enum", location
    return _schema_val(schema, path)


def _schema_val(schema: Any, path: list[str]) -> Any:
    from jschon import JSON, JSONSchema

    assert isinstance(schema, JSONSchema)
    cur = dict(schema)
    missing = object()
    for i, part in enumerate(path):
        if part == "$ref":
            assert isinstance(cur, dict)
            cur = _resolve_ref(cur.get("$ref"), schema)
            continue
        if isinstance(cur, JSON):
            cur = cur.value
        if isinstance(cur, dict):
            assert isinstance(part, str)
            cur = cur.get(part, missing)
            assert not cur is missing, (part, i, path)
        elif isinstance(cur, list):
            try:
                index = int(part)
            except ValueError:
                assert False, (part, i, path)
            else:
                assert index >= 0 and index < len(cur), (part, i, path)
                cur = cur[index]
        else:
            assert False, (part, i, path, cur)
    return cur


def _resolve_ref(ref: Any, schema: Any):
    assert isinstance(ref, str) and ref.startswith("#/"), ref
    return _schema_val(schema, ref[2:].split("/"))
