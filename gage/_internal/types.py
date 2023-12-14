# SPDX-License-Identifier: Apache-2.0

from typing import *

__all__ = [
    "CmdArgs",
    "GageFile",
    "GageFileError",
    "GageFileLoadError",
    "GageFileValidationError",
    "JSONCompatible",
    "OpCmd",
    "OpDef",
    "OpDefNotFound",
    "OpRef",
    "OpDefExec",
    "OpDefConfig",
    "Repository",
    "Run",
    "RunComment",
    "RunConfig",
    "RunConfigValue",
    "RunContext",
    "RunStatus",
    "RunTimestamp",
    "SchemaValidationError",
    "UnifiedDiff",
    "UserConfig",
    "UserConfigError",
    "UserConfigLoadError",
    "UserConfigValidationError",
]

Data = dict[str, Any]

# NO IMPORTS ALLOWED


# =================================================================
# Errors
# =================================================================


class SchemaValidationError(Protocol):
    validation_result: Any


class OpDefNotFound(Exception):
    def __init__(self, opspec: str | None, filename: str):
        self.opspec = opspec
        self.filename = filename


class GageFileError(Exception):
    pass


class GageFileLoadError(GageFileError):
    def __init__(self, filename: str, msg: Any):
        self.filename = filename
        self.msg = msg


class GageFileValidationError(GageFileError):
    def __init__(self, validation_result: Any):
        super().__init__(validation_result)
        self.validation_result = validation_result


class UserConfigError(Exception):
    pass


class UserConfigLoadError(UserConfigError):
    def __init__(self, filename: str, msg: Any):
        self.filename = filename
        self.msg = msg


class UserConfigValidationError(UserConfigError):
    def __init__(self, validation_result: Any):
        super().__init__(validation_result)
        self.validation_result = validation_result


# =================================================================
# Gage file
# =================================================================


class OpRef:
    def __init__(self, op_ns: str, op_name: str):
        self.op_ns = op_ns
        self.op_name = op_name

    def get_full_name(self):
        if not self.op_ns:
            return self.op_name
        return f"{self.op_ns}:{self.op_name}"

    def __repr__(self):
        return f"<OpRef ns=\"{self.op_ns}\" name=\"{self.op_name}\">"


CmdArgs = str | list[str]


class OpCmd:
    def __init__(self, args: CmdArgs, env: dict[str, str]):
        self.args = args
        self.env = env


class OpDefExec:
    def __init__(self, data: Data):
        self._data = data

    def as_json(self) -> Data:
        return self._data

    def get_stage_sourcecode(self) -> CmdArgs | None:
        return self._data.get("stage-sourcecode")

    def get_stage_dependencies(self) -> CmdArgs | None:
        return self._data.get("stage-dependencies")

    def get_stage_runtime(self) -> CmdArgs | None:
        return self._data.get("stage-runtime")

    def get_run(self) -> CmdArgs | None:
        return self._data.get("run")

    def get_finalize_run(self) -> CmdArgs | None:
        return self._data.get("finalize-run")


class OpDefConfig:
    def __init__(self, data: Data):
        self._data = data

    def as_json(self) -> Data:
        return self._data

    def get_description(self) -> str | None:
        return self._data.get("description")

    def get_keys(self) -> list[str]:
        val = self._data.get("keys")
        if val is None:
            return []
        if isinstance(val, str):
            return [val]
        return val


class OpDefDependency:
    def __init__(self, data: Data):
        self._data = data

    def as_json(self) -> Data:
        return self._data

    def get_type(self):
        type = self._data.get("type")
        if type:
            return type
        if self._data.get("run-select"):
            return "run-files"
        if self._data.get("files"):
            return "project-files"
        return ""


class OpDef:
    def __init__(self, name: str, data: Data, src: str | None = None):
        self.name = name
        self._data = data
        self._src = src

    def as_json(self) -> Data:
        return self._data

    def get_src(self):
        if self._src is None:
            raise TypeError(
                "OpDef was not created with src - read src attribute "
                "directly to bypass this check"
            )
        return self._src

    def get_description(self) -> str | None:
        return self._data.get("description")

    def get_default(self):
        return bool(self._data.get("default"))

    def get_exec(self) -> OpDefExec:
        val = self._data.get("exec")
        if val is None:
            val = {}
        elif isinstance(val, str) or isinstance(val, list):
            val = {"run": val}
        return OpDefExec(val)

    def get_sourcecode(self) -> list[str] | bool | None:
        val = self._data.get("sourcecode")
        if val in (True, False, None):
            return val
        if isinstance(val, str):
            return [val]
        return val

    def get_config(self) -> list[OpDefConfig]:
        val = self._data.get("config")
        if val is None:
            val = []
        elif isinstance(val, str):
            val = [{"keys": val}]
        elif isinstance(val, dict):
            val = [val]
        return [OpDefConfig(item) for item in val]

    def get_dependencies(self) -> list[OpDefDependency]:
        val = self._data.get("depends")
        if val is None:
            val = []
        elif isinstance(val, dict):
            val = [val]
        return [OpDefDependency(item) for item in val]


class GageFile:
    def __init__(self, filename: str, data: Data):
        self.filename = filename
        self._data = data

    def as_json(self) -> Data:
        return self._data

    def get_operations(self):
        return {
            name: OpDef(name, self._data[name], self.filename)  # \
            for name in self._data
        }


# =================================================================
# User config
# =================================================================


class Repository:
    def __init__(self, name: str, data: dict[str, Any]):
        self.name = name
        self._data = data

    def as_json(self) -> Data:
        return self._data

    def get_type(self) -> str:
        return self._data.get("type", "local")

    def attrs(self):
        return {name: self._data[name] for name in self._data if name != "type"}


class UserConfig:
    def __init__(self, filename: str, data: Data):
        self.filename = filename
        self.parent: UserConfig | None = None
        self._data = data

    def as_json(self) -> Data:
        return self._data

    def get_repositories(self):
        repos_data = self._data.get("repos", {})
        repos = {name: Repository(name, repos_data[name]) for name in repos_data}
        if self.parent:
            parent_repos = self.parent.get_repositories()
            for name in parent_repos:
                if name not in repos:
                    repos[name] = parent_repos[name]
        return repos


def _repo_name(data: dict[str, Any]):
    return data.get("name") or data.get("type") or "local"


# =================================================================
# Run
# =================================================================


class RunContext(NamedTuple):
    command_dir: str
    project_dir: str
    gagefile: GageFile
    opref: OpRef
    opdef: OpDef


class Run:
    def __init__(self, id: str, opref: OpRef, meta_dir: str, run_dir: str, name: str):
        self.id = id
        self.opref = opref
        self.meta_dir = meta_dir
        self.run_dir = run_dir
        self.name = name
        self._cache: dict[str, Any] = {}

    def __repr__(self):
        return f"<Run id=\"{self.id}\" name=\"{self.name}\">"


RunStatus = Literal[
    "running",
    "completed",
    "error",
    "terminated",
    "staged",
    "pending",
    "unknown",
]

RunTimestamp = Literal[
    "initialized",
    "staged",
    "started",
    "stopped",
]


RunConfigValue = None | int | float | bool | str


class RunConfig(dict[str, RunConfigValue]):
    _initialized = False

    def __setitem__(self, key: str, item: RunConfigValue):
        if self._initialized and key not in self:
            raise KeyError(key)
        super().__setitem__(key, item)

    def apply(self) -> str:
        """Applies config returning the new source."""
        raise NotImplementedError()


class RunComment(NamedTuple):
    id: str
    author: str
    timestamp: int
    msg: str


# =================================================================
# General
# =================================================================


UnifiedDiff = list[str]

JSONCompatible = None | bool | int | float | str | Sequence[Any] | Mapping[str, Any]
