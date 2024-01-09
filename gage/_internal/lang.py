# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

from lark import *

from . import run_filter

__all__ = [
    "parse_config_value",
    "parse_flag_assign",
    "parse_where_expr",
]

GRAMMAR = r"""
flag_assign: config_key WS_INLINE? "=" WS_INLINE? config_value? -> flag_assign

config_key: KEY            -> key
          | QUOTED_STRING  -> quoted_string

config_value: QUOTED_STRING   -> quoted_string
            | "true"          -> true
            | "false"         -> false
            | "null"          -> null
            | ANY             -> dynamic

where_expr: ANY   -> run_string_match

_STRING_INNER: /.*?/
_STRING_ESC_INNER: _STRING_INNER /(?<!\\)(\\\\)*?/

_QUOTED_STRING_DOUBLE: "\"" _STRING_ESC_INNER "\""
_QUOTED_STRING_SINGLE: "'" _STRING_ESC_INNER "'"

QUOTED_STRING: _QUOTED_STRING_DOUBLE | _QUOTED_STRING_SINGLE

ANY: /.+/

KEY: /[\w_\.][\w_\.\-]*/

%import common.SIGNED_INT
%import common.SIGNED_FLOAT
%import common.LETTER
%import common.DIGIT
%import common.WS_INLINE

%ignore WS_INLINE
"""


def parse_config_value(s: str) -> RunConfigValue:
    s = s.strip()
    if not s:
        return s
    p = _ensure_config_val_parser()
    return cast(RunConfigValue, _parse(p, s))


__config_val_parser: Lark | None = None


def _ensure_config_val_parser() -> Lark:
    if __config_val_parser is None:
        globals()["__config_val_parser"] = Lark(
            GRAMMAR,
            start="config_value",
            parser="lalr",
            transformer=GrammarTransformer(),
        )
    assert __config_val_parser
    return __config_val_parser


def _parse(parser: Lark, s: str):
    try:
        return parser.parse(s)
    except UnexpectedCharacters as e:
        raise ValueError(
            f"Unexpected character '{e.char}' at line {e.line} col {e.column}"
        ) from None
    except UnexpectedToken as e:
        if e.token == "":
            raise ValueError(
                f"Missing expected token: one of {', '.join(sorted(e.expected))}"
            ) from None
        raise ValueError(f"Unexpected token: {e.token.value!r}") from None


def parse_flag_assign(s: str) -> tuple[str, RunConfigValue]:
    s = s.strip()
    p = _ensure_flag_assign_parser()
    return cast(tuple[str, RunConfigValue], _parse(p, s))


__flag_assign_parser: Lark | None = None


def _ensure_flag_assign_parser() -> Lark:
    if __flag_assign_parser is None:
        globals()["__flag_assign_parser"] = Lark(
            GRAMMAR,
            start="flag_assign",
            parser="lalr",
            transformer=GrammarTransformer(),
        )
    assert __flag_assign_parser
    return __flag_assign_parser


def parse_where_expr(s: str) -> RunFilter:
    s = s.strip()
    p = _ensure_where_expr_parser()
    return cast(RunFilter, _parse(p, s))


__where_expr_parser: Lark | None = None


def _ensure_where_expr_parser() -> Lark:
    if __where_expr_parser is None:
        globals()["__where_expr_parser"] = Lark(
            GRAMMAR,
            start="where_expr",
            parser="lalr",
            transformer=GrammarTransformer(),
        )
    assert __where_expr_parser
    return __where_expr_parser


class GrammarTransformer(Transformer):
    def int(self, tokens: list[Token]):
        (s,) = tokens
        return int(s)

    def float(self, tokens: list[Token]):
        (s,) = tokens
        return float(s)

    def quoted_string(self, tokens: list[Token]):
        (s,) = tokens
        return s[1:-1]

    def true(self, tokens: list[Token]):
        return True

    def false(self, tokens: list[Token]):
        return False

    def null(self, tokens: list[Token]):
        return None

    def dynamic(self, tokens: list[Token]):
        (s,) = tokens
        try:
            return int(s.value)
        except ValueError:
            try:
                return float(s.value)
            except ValueError:
                return s.value

    def flag_assign(self, tokens: list[Token]):
        assert len(tokens) in (1, 2), tokens
        return (tokens[0], "") if len(tokens) == 1 else tuple(tokens)

    def key(self, tokens: list[Token]):
        (s,) = tokens
        return s.value

    def run_string_match(self, tokens: list[Token]):
        (s,) = tokens
        return run_filter.string_match_filter(s)
