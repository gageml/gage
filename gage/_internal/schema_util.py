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
    return list(e.validation_result.collect_errors())
