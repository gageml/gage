# SPDX-License-Identifier: Apache-2.0

from typing import *

import ast
import logging
import os
import re
import types

try:
    # pylint: disable=ungrouped-imports
    from ast import NameConstant
except ImportError:

    class FakeType:
        pass

    NameConstant = FakeType

log = logging.getLogger()


class Script:
    def __init__(
        self,
        src: str,
        mod_package: Optional[str] = None,
        sys_path: Optional[str] = None,
    ):
        self.src = src
        self.name = _script_name(src)
        self.mod_package = mod_package
        self.sys_path = sys_path
        self._parsed = False
        self._imports: list[str] = []
        self._calls: list[Call] = []
        self._params: dict[str, Any] = {}
        self._parse()

    def __lt__(self, x: Any):
        return self.__cmp__(x) < 0

    def __cmp__(self, x: Any):
        return (self.src > x.src) - (self.src < x.src)

    @property
    def imports(self):
        return self._imports

    @property
    def calls(self):
        return self._calls

    @property
    def params(self):
        return self._params

    def _parse(self):
        assert not self._parsed
        parsed = ast.parse(open(self.src, "r").read())
        for node in ast.walk(parsed):
            self._safe_apply_node(node)
        self._parsed = True

    def _safe_apply_node(self, node: ast.AST):
        try:
            self._apply_node(node)
        except Exception as e:
            self._handle_apply_node_error(e, node)

    def _handle_apply_node_error(self, e: Exception, node: ast.AST):
        msg = (
            f"error applying AST node {node.__class__.__name__} "
            f"from {self.src}:{node.lineno}:"
        )
        if log.getEffectiveLevel() <= logging.DEBUG:
            log.exception(msg)
        else:
            msg += f" {e} (use 'guild --debug ...' for more information)"
            log.warning(msg)

    def _apply_node(self, node: ast.AST):
        if isinstance(node, ast.ImportFrom):
            self._apply_import_from(node)
        elif isinstance(node, ast.Import):
            self._apply_import(node)
        elif isinstance(node, ast.Call):
            self._maybe_apply_call(node)
        elif isinstance(node, ast.Assign):
            self._apply_assign(node)

    def _apply_import_from(self, node: ast.ImportFrom):
        if node.module:
            self._imports.append(node.module)
        if node.names:
            self._apply_import(node)

    def _apply_import(self, node: ast.Import | ast.ImportFrom):
        for name in node.names:
            if isinstance(name, ast.alias):
                self._imports.append(name.name)

    def _maybe_apply_call(self, node: ast.Call):
        call = Call(node)
        if call.name:
            self._calls.append(call)

    def _apply_assign(self, node: ast.Assign):
        if node.col_offset == 0:
            self._try_apply_param(node)

    def _try_apply_param(self, node: ast.Assign):
        try:
            val = ast_param_val(node.value)
        except TypeError:
            pass
        else:
            for target in node.targets:
                if not isinstance(target, ast.Name):
                    continue
                # Ignore subsequent assigns to target. See #442
                if target.id in self._params:
                    continue
                self._params[target.id] = val


def ast_param_val(val: ast.AST) -> Any:
    if isinstance(val, ast.Num):
        return val.n
    if isinstance(val, ast.Str):
        return val.s
    if isinstance(val, NameConstant):
        return val.value
    if isinstance(val, ast.Name):
        if val.id == "True":
            return True
        if val.id == "False":
            return False
        if val.id == "None":
            return None
        raise TypeError(val)
    if isinstance(val, ast.List):
        return [ast_param_val(item) for item in val.elts]
    if isinstance(val, ast.UnaryOp):
        return _unary_val(val)
    if isinstance(val, ast.Dict):
        return {
            ast_param_val(item_key): ast_param_val(item_val)
            for item_key, item_val in zip(val.keys, val.values)
            if item_key
        }
    if isinstance(val, ast.Call) and _call_is_namespace(val):
        return _namespace_val(val)
    raise TypeError(val)


def _unary_val(val: ast.UnaryOp):
    if isinstance(val.operand, ast.Num):
        if isinstance(val.op, ast.USub):
            return -val.operand.n
        if isinstance(val.op, ast.UAdd):
            return +val.operand.n
        raise TypeError(val)
    raise TypeError(val)


def _call_is_namespace(val: ast.Call):
    return isinstance(val.func, ast.Name) and val.func.id.endswith("SimpleNamespace")


def _namespace_val(val: ast.Call):
    kw = _namespace_kw(val)
    return types.SimpleNamespace(**kw)


def _namespace_kw(val: ast.Call) -> dict[str, Any]:
    return {kw.arg: ast_param_val(kw.value) for kw in val.keywords if kw.arg}


class Call:
    def __init__(self, node: ast.Call):
        self.node = node
        self.name = self._func_name(node.func)

    def _func_name(self, func: ast.expr) -> str | None:
        if isinstance(func, ast.Name):
            return func.id
        if isinstance(func, ast.Attribute):
            return func.attr
        if isinstance(func, ast.Call):
            return self._func_name(func.func)
        if isinstance(func, ast.Subscript):
            return None
        raise AssertionError(func)

    def kwarg_param(self, name: str):
        for kw in self.node.keywords:
            if kw.arg == name:
                try:
                    ast_param_val(kw.value)
                except TypeError:
                    return None
        return None


def _script_name(src: str):
    basename = os.path.basename(src)
    name, _ext = os.path.splitext(basename)
    return name


class Result(Exception):
    def __init__(self, value: Any):
        super().__init__(value)
        self.value = value


_Func = Callable[..., Any]
_Class = Type[Any]
_Callback = Callable[..., None]
_Method = Callable[..., Any]
_Module = Any


class MethodWrapper:
    @staticmethod
    def for_method(method: _Method):
        return getattr(method, "__wrapper__", None)

    def __init__(self, func: _Func, cls: _Class, name: str):
        self._func = func
        self._cls = cls
        self._name = name
        self._cbs = []
        self._wrap()

    def _wrap(self):
        wrapper = self._wrapper()
        wrapper.__name__ = f"{self._name}_wrapper"
        wrapper.__wrapper__ = self
        setattr(self._cls, self._name, wrapper)

    def _wrapper(self):
        def wrapper(wrapped_self: MethodWrapper, *args: Any, **kw: Any):
            wrapped_bound = self._bind(wrapped_self)
            marker = object()
            result = marker
            for cb in self._cbs:
                try:
                    cb(wrapped_bound, *args, **kw)
                except Result as e:
                    result = e.value
            if result is marker:
                return wrapped_bound(*args, **kw)
            return result

        return wrapper

    def _bind(self, wrapped_self: Any):
        def f(*args: Any, **kw: Any):
            return self._func(wrapped_self, *args, **kw)

        f.__self__ = wrapped_self
        return f

    def add_cb(self, cb: _Callback):
        self._cbs.append(cb)

    def remove_cb(self, cb: _Callback):
        try:
            self._cbs.remove(cb)
        except ValueError:
            pass
        if not self._cbs:
            self.unwrap()

    def unwrap(self):
        setattr(self._cls, self._name, self._func)


def wrapped_self(method_wrapper: _Func):
    return method_wrapper.__self__


def listen_method(cls: _Class, method_name: str, cb: _Callback):
    method = getattr(cls, method_name)
    wrapper = MethodWrapper.for_method(method)
    if wrapper is None:
        wrapper = MethodWrapper(method, cls, method_name)
    wrapper.add_cb(cb)
    return wrapper


def remove_method_listener(method: _Method, cb: _Callback):
    wrapper = MethodWrapper.for_method(method)
    if wrapper is not None:
        wrapper.remove_cb(cb)


def remove_method_listeners(method: _Method):
    wrapper = MethodWrapper.for_method(method)
    if wrapper is not None:
        wrapper.unwrap()


class FunctionWrapper:
    @staticmethod
    def for_function(function: _Func):
        return getattr(function, "__wrapper__", None)

    def __init__(self, func: _Func, mod: _Module, name: str):
        self._func = func
        self._mod = mod
        self._name = name
        self._cbs = []
        self._wrap()

    def _wrap(self):
        wrapper = self._wrapper()
        wrapper.__name__ = f"{self._name}_wrapper"
        wrapper.__wrapper__ = self
        setattr(self._mod, self._name, wrapper)

    def _wrapper(self):
        def wrapper(*args: Any, **kw: Any):
            marker = object()
            result = marker
            for cb in self._cbs:
                try:
                    cb(self._func, *args, **kw)
                except Result as e:
                    result = e.value
            if result is marker:
                return self._func(*args, **kw)
            return result

        return wrapper

    def add_cb(self, cb: _Callback):
        self._cbs.append(cb)

    def remove_cb(self, cb: _Callback):
        try:
            self._cbs.remove(cb)
        except ValueError:
            pass
        if not self._cbs:
            self.unwrap()

    def unwrap(self):
        setattr(self._mod, self._name, self._func)


def listen_function(module: _Module, function_name: str, cb: _Callback):
    function = getattr(module, function_name)
    wrapper = FunctionWrapper.for_function(function)
    if wrapper is None:
        wrapper = FunctionWrapper(function, module, function_name)
    wrapper.add_cb(cb)


def remove_function_listener(function: _Func, cb: _Callback):
    wrapper = FunctionWrapper.for_function(function)
    if wrapper is not None:
        wrapper.remove_cb(cb)


def remove_function_listeners(function: _Func):
    wrapper = FunctionWrapper.for_function(function)
    if wrapper is not None:
        wrapper.unwrap()


def scripts_for_dir(dir: str, exclude: Optional[list[str]] = None):
    import glob
    import fnmatch

    exclude = exclude or []
    return [
        Script(src)
        for src in glob.glob(os.path.join(dir, "*.py"))
        if not any((fnmatch.fnmatch(src, pattern) for pattern in exclude))
    ]


_Globals = dict[str, Any]


def exec_script(
    filename: str, globals: Optional[_Globals] = None, mod_name: str = "__main__"
):
    """Execute a Python script.

    This function can be used to execute a Python module as code
    rather than import it. Importing a module to execute it is not an
    option if importing has a side-effect of starting threads, in
    which case this function can be used.

    `mod_name` is ``__main__`` by default but may be an alternative
    module name. `mod_name` may include a package.

    Reference:

    https://docs.python.org/2/library/threading.html#importing-in-threaded-code

    """
    globals = globals or {}
    package_name, mod_name = split_mod_name(mod_name)
    _ensure_parent_mod_loaded(package_name)
    node_filter = _node_filter_for_globals(globals) if globals else None
    src = open(filename, "r").read()
    code = _compile_script(src, filename, node_filter)
    script_globals = dict(globals)
    script_globals.update(
        {
            "__package__": package_name,
            "__name__": mod_name,
            "__file__": filename,
        }
    )
    # pylint: disable=exec-used
    exec(code, script_globals)
    return script_globals


def split_mod_name(mod_name: str):
    parts = mod_name.split(".")
    return ".".join(parts[:-1]), parts[-1]


def _ensure_parent_mod_loaded(parent_mod_name: str):
    if parent_mod_name:
        try:
            __import__(parent_mod_name)
        except ValueError:
            assert False, parent_mod_name


def _node_filter_for_globals(globals: _Globals):
    """Filters ast nodes in support of setting globals for exec.

    Removes initial assigns of any variables occurring in
    `globals`. This is to allow globals to provide the initial
    value. Subsequent assigns are not removed under the assumption
    that are re-defining the initial variable value.
    """
    names = set(globals.keys())
    removed = set()

    def f(node: ast.AST):
        if isinstance(node, ast.Assign):
            for target in node.targets:
                if not isinstance(target, ast.Name) or target.id in removed:
                    return True
                if target.id in names:
                    removed.add(target.id)
                    return False
        return True

    return f


_NodeFilter = Callable[[ast.AST], bool]


def _compile_script(src: str, filename: str, node_filter: Optional[_NodeFilter] = None):
    import __future__

    ast_root = ast.parse(src, filename)
    if node_filter:
        ast_root = _filter_nodes(ast_root, node_filter)
    flags = __future__.absolute_import.compiler_flag
    return compile(
        cast(ast.Module, ast_root), filename, "exec", flags=flags, dont_inherit=True
    )


def _filter_nodes(root: ast.AST, node_filter: _NodeFilter):
    if isinstance(root, (ast.Module, ast.If)):
        root.body = [
            cast(ast.stmt, _filter_nodes(node, node_filter))
            for node in root.body
            if node_filter(node)
        ]
    return root


_Attrs = dict[str, Any]
_RefSpec = tuple[str, Type[Any], _Attrs]


def update_refs(
    module: _Module,
    ref_spec: _RefSpec,
    new_val: Any,
    recurse: bool = False,
    seen: Optional[Set[_Module]] = None,
):
    seen = seen or set()
    if module in seen:
        return
    seen.add(module)
    for name, val in module.__dict__.items():
        if _match_ref(name, val, ref_spec):
            module.__dict__[name] = new_val
        elif recurse and isinstance(val, types.ModuleType):
            update_refs(val, ref_spec, new_val, recurse, seen)


def _match_ref(name: str, val: Any, ref_spec: _RefSpec):
    target_name, target_type, target_attrs = ref_spec
    return (
        name == target_name
        and isinstance(val, target_type)
        and _match_ref_attrs(val, target_attrs)
    )


def _match_ref_attrs(val: Any, attrs: _Attrs):
    undef = object()
    return all((getattr(val, name, undef) == attrs[name] for name in attrs))


def is_python_script(opspec: str):
    return os.path.isfile(opspec) and opspec[-3:] == ".py"


def safe_module_name(s: str):
    if s.lower().endswith(".py"):
        s = s[:-3]
    return re.sub("-", "_", s)


def check_package_version(version: str, req: str):
    parsed_req = _parse_req_for_version_spec(req)
    matches = list(parsed_req.specifier.filter({version: ""}, prereleases=True))
    return len(matches) > 0


def _parse_req_for_version_spec(version_spec: str):
    import pkg_resources

    version_spec = _maybe_apply_equals(version_spec)
    if version_spec[:1] not in "=<>":
        raise ValueError(
            f"Invalid version spec {version_spec!r}: missing comparison "
            "operator (==, <, >, etc.)"
        )
    try:
        return pkg_resources.Requirement.parse(f"fakepkg{version_spec}")
    except Exception as e:
        # assert exception type name rather than handle explicitly
        # (API changed in 67.5.1, breaking backward compatibility)
        assert e.__class__.__name__ in (
            "InvalidRequirement",
            "RequirementParseError",
        ), e.__class__
        raise ValueError(
            f"Invalid version spec {version_spec!r}: {_norm_e(e)}"
        ) from None


def _norm_e(e: Exception):
    msg = str(e)
    return msg[:1].lower() + msg[1:]


def _maybe_apply_equals(version_spec: str):
    return ",".join(
        [_maybe_apply_equals_to_version(part) for part in version_spec.split(",")]
    )


def _maybe_apply_equals_to_version(s: str):
    return f"=={s}" if re.match(r"^\d", s) else s


def first_breakable_line(src: str):
    return next_breakable_line(src, 1)


def next_breakable_line(src: str, line: int = 1):
    """Returns the next breaking line in a Python module.

    Pdb breakpoints require non-expression lines to take
    effect. I.e. they require something to execute. A non-breakable
    line is ignored.

    This function returns the next breakable line starting with
    `line`. If `line` is breakable, it is returned.

    If `src` is not a valid Python module or the module doesn't
    contain a breakable line, the function raises TypeError.
    """
    parsed = ast.parse(open(src, "r").read())
    for lineno in _iter_breakable_lines(parsed):
        if lineno >= line:
            return lineno
    raise TypeError(f"no breakable lines at or after {line} in {src}")


def _iter_breakable_lines(top: ast.AST) -> Generator[int, None, None]:
    for node in ast.walk(top):
        if node is top:
            continue
        line = getattr(node, "lineno", None)
        if line is None:
            continue
        if _is_node_breakable(node):
            yield line
        for line in _iter_breakable_lines(node):
            yield line


NON_BREAKABLE_NODE_TYPES: Set = {
    ast.Expr,
    ast.Str,
    ast.Num,
    ast.List,
    ast.Dict,
    ast.Tuple,
    ast.Set,
    ast.Name,
}

try:
    NON_BREAKABLE_NODE_TYPES.add(ast.Constant)
except AttributeError:
    pass


def _is_node_breakable(node: Any):
    return type(node) not in NON_BREAKABLE_NODE_TYPES
