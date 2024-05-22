# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import OpRef

__all__ = ["encode_opref", "decode_opref"]

OPREF_ENCODING_SCHEMA = '2'


def encode_opref(opref: OpRef):
    if not opref.op_ns:
        raise ValueError("opref namespace (op_ns) cannot be empty")
    if " " in opref.op_ns:
        raise ValueError("opref namespace (op_ns) cannot contain spaces")
    if not opref.op_name:
        raise ValueError("opref name (op_name) cannot be empty")
    if " " in opref.op_name:
        raise ValueError("opref name (op_name) cannot contain spaces")
    if opref.op_version and " " in opref.op_version:
        raise ValueError("opref version (op_version) cannot contain spaces")
    version = f" {opref.op_version}" if opref.op_version else ""
    return f"{OPREF_ENCODING_SCHEMA} {opref.op_ns} {opref.op_name}{version}"


def decode_opref(s: str):
    [schema, *rest] = s.rstrip().split(" ", 1)
    if not schema or not rest:
        raise ValueError(f"invalid opref encoding: {s!r}")
    if schema == OPREF_ENCODING_SCHEMA:
        return _decode_v2_opref(rest, s)
    elif schema == "1":
        return _decode_v1_opref(rest, s)
    else:
        raise ValueError(f"unsupported opref schema: {s!r}")


def _decode_v2_opref(parts: list[str], encoded: str):
    [ns, *rest] = parts[0].split(" ")
    if not ns or len(rest) not in (1, 2):
        raise ValueError(f"invalid opref encoding: {encoded!r}")
    return OpRef(ns, rest[0], rest[1] if len(rest) == 2 else None)


def _decode_v1_opref(parts: list[str], encoded: str):
    [ns, *rest] = parts[0].split(" ")
    if not ns or len(rest) != 1:
        raise ValueError(f"invalid opref encoding: {encoded!r}")
    return OpRef(ns, rest[0])
