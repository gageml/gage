# SPDX-License-Identifier: Apache-2.0

from typing import *

import libcst as cst

from .run_config import *

__all__ = ["PythonConfig"]


KeyNodes = dict[str, cst.CSTNode]


class PythonConfig(RunConfig):
    def __init__(self, source: str):
        super().__init__()
        self._cst = cst.parse_module(source)
        self._key_nodes: KeyNodes = {}
        _init_config(self._cst, self, self._key_nodes)
        self._initialized = True

    def apply(self):
        modified = _apply_config(self._cst, self, self._key_nodes)
        return modified.code


def _init_config(module: cst.Module, config: RunConfig, key_nodes: KeyNodes):
    visitor = ConfigVisitor(config, key_nodes)
    module.visit(visitor)


def _apply_config(module: cst.Module, config: RunConfig, key_nodes: KeyNodes):
    transformer = ConfigTransformer(config, key_nodes)
    return module.visit(transformer)


class ConfigVisitor(cst.CSTVisitor):
    def __init__(self, config: RunConfig, key_nodes: KeyNodes):
        self._config = config
        self._key_nodes = key_nodes

    def on_visit(self, node: cst.CSTNode):
        """Traverses the module to find top-level value assignments.

        Value assignments must be of config value types (numbers,
        strings, etc.)

        Skips further traversal by returning False.
        """
        module = cst.ensure_type(node, cst.Module)
        for assign in _iter_top_level_assigns(module):
            _apply_assign(assign, self._config, self._key_nodes)
        return False


def _iter_top_level_assigns(module: cst.Module):
    for node in module.body:
        if isinstance(node, cst.SimpleStatementLine):
            for stmt_node in node.body:
                if isinstance(stmt_node, cst.Assign):
                    yield stmt_node


def _apply_assign(assign: cst.Assign, config: RunConfig, key_nodes: KeyNodes):
    for name in _iter_target_names(assign):
        _apply_key_val(name, assign.value, config, key_nodes)


def _iter_target_names(assign: cst.Assign):
    for t in assign.targets:
        if isinstance(t.target, cst.Name):
            yield t.target.value


def _apply_key_val(
    key: str, val: cst.BaseExpression, config: RunConfig, key_nodes: KeyNodes
):
    try:
        simple_val = _simple_val(val)
    except TypeError:
        if isinstance(val, cst.List):
            _apply_key_list_val(key, val, config, key_nodes)
        elif isinstance(val, cst.Dict):
            _apply_key_dict_val(key, val, config, key_nodes)
    else:
        config[key] = simple_val
        key_nodes[key] = val


_NAMES: dict[str, Literal[True, False, None]] = {
    "True": True,
    "False": False,
    "None": None,
}


def _simple_val(val: cst.BaseExpression):
    if isinstance(val, cst.Integer):
        return int(val.value)
    if isinstance(val, cst.Float):
        return float(val.value)
    if isinstance(val, cst.SimpleString):
        return val.value[1:-1]
    if isinstance(val, cst.Name):
        try:
            return _NAMES[val.value]
        except KeyError:
            raise TypeError()
    raise TypeError()


def _apply_key_list_val(key: str, l: cst.List, config: RunConfig, key_nodes: KeyNodes):
    for i, element in enumerate(l.elements):
        _apply_key_val(f"{key}.{i}", element.value, config, key_nodes)


def _apply_key_dict_val(key: str, d: cst.Dict, config: RunConfig, key_nodes: KeyNodes):
    for element in d.elements:
        if not isinstance(element, cst.DictElement):
            continue
        if not isinstance(element.key, cst.SimpleString):
            continue
        el_key = element.key.value[1:-1]
        _apply_key_val(f"{key}.{el_key}", element.value, config, key_nodes)


class ConfigTransformer(cst.CSTTransformer):
    def __init__(self, config: RunConfig, key_nodes: KeyNodes):
        self._config = config
        self._key_nodes = key_nodes
        self._replacements: dict[cst.CSTNode, cst.CSTNode] = {}

    def visit_Module(self, module: cst.Module):
        """Traverses the module to apply config values.

        Creates a lookup of nodes and their replacements, which is used
        by `on_leave()` to return the applicable updated node.
        """
        for assign in _iter_top_level_assigns(module):
            self._replacements.update(
                _iter_config_replacements(self._config, self._key_nodes)
            )

    def on_leave(self, original_node: cst.CSTNode, updated_node: cst.CSTNode):
        return self._replacements.get(original_node) or updated_node


def _iter_config_replacements(
    config: RunConfig, key_nodes: KeyNodes
) -> Generator[tuple[cst.CSTNode, cst.CSTNode], Any, None]:
    for key in key_nodes:
        try:
            config_val = config[key]
        except KeyError:
            pass
        else:
            orig_node = key_nodes[key]
            enc_config_val = _encode_config_val(config_val, orig_node)
            yield orig_node, orig_node.with_changes(value=enc_config_val)


def _encode_config_val(val: Any, node: cst.CSTNode):
    if isinstance(node, cst.SimpleString):
        return _encode_string(val, node.value[0])
    return repr(val)


def _encode_string(val: str, prefix: str):
    assert prefix in ("\"", "'"), prefix
    escaped = val.replace("\"", "\\\"") if prefix == "\"" else val.replace("'", "\\'")
    return prefix + escaped + prefix
