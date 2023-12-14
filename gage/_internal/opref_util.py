# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import OpRef

__all__ = ["encode_opref", "decode_opref"]

OPREF_ENCODING_SCHEMA = '1'


def encode_opref(opref: OpRef):
    if not opref.op_ns:
        raise ValueError("opref namespace (op_ns) cannot be empty")
    if " " in opref.op_ns:
        raise ValueError("opref namespace (op_ns) cannot contain spaces")
    if not opref.op_name:
        raise ValueError("opref name (op_name) cannot be empty")
    if " " in opref.op_name:
        raise ValueError("opref name (op_name) cannot contain spaces")
    return f"{OPREF_ENCODING_SCHEMA} {opref.op_ns} {opref.op_name}"


def decode_opref(s: str):
    [schema, *rest] = s.rstrip().split(" ", 1)
    if not schema or not rest:
        raise ValueError(f"invalid opref encoding: {s!r}")
    if schema != OPREF_ENCODING_SCHEMA:
        raise ValueError(f"unsupported opref schema: {s!r}")
    [ns, *rest] = rest[0].split(" ", 1)
    if not ns or not rest:
        raise ValueError(f"invalid opref encoding: {s!r}")
    return OpRef(ns, rest[0])
