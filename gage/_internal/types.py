# SPDX-License-Identifier: Apache-2.0

from typing import *

__all__ = [
    "ArchiveDef",
    "BoardConfigError",
    "BoardConfigLoadError",
    "BoardConfigValidationError",
    "BoardDef",
    "BoardDefColumn",
    "BoardDefRunSelect",
    "BoardDefGroupSelect",
    "CmdArgs",
    "GageFile",
    "GageFileError",
    "GageFileLoadError",
    "GageFileValidationError",
    "IndexedRun",
    "JSONCompatible",
    "OpCmd",
    "OpDef",
    "OpDefConfig",
    "OpDefDependency",
    "OpDefExec",
    "OpDefNotFound",
    "OpDefSummary",
    "OpRef",
    "Repository",
    "Run",
    "RunComment",
    "RunConfig",
    "RunConfigValue",
    "RunContext",
    "RunFilter",
    "RunProxy",
    "RunStatus",
    "RunSummary",
    "RunTimestamp",
    "SchemaValidationError",
    "UnifiedDiff",
    "UserConfig",
    "UserConfigError",
    "UserConfigLoadError",
    "UserConfigValidationError",
]

Data = dict[str, Any]

# NO IMPORTS ALLOWED!


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
    def __init__(self, filename: str, msg: str):
        self.filename = filename
        self.msg = msg


class GageFileLoadError(GageFileError):
    pass


class GageFileValidationError(Exception):
    def __init__(self, validation_result: Any):
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


class BoardConfigError(Exception):
    pass


class BoardConfigLoadError(BoardConfigError):
    def __init__(self, filename: str, msg: Any):
        self.filename = filename
        self.msg = msg


class BoardConfigValidationError(BoardConfigError):
    def __init__(self, validation_result: Any):
        super().__init__(validation_result)
        self.validation_result = validation_result


# =================================================================
# Gage file
# =================================================================


class OpRef:
    def __init__(self, op_ns: str, op_name: str, op_version: str | None = None):
        self.op_ns = op_ns
        self.op_name = op_name
        self.op_version = op_version

    def get_full_name(self):
        if not self.op_ns:
            return self.op_name
        return f"{self.op_ns}:{self.op_name}"

    def __repr__(self):
        version = (
            f" version=\"{self.op_version}\"" if self.op_version is not None else ""
        )
        return f"<OpRef ns=\"{self.op_ns}\" name=\"{self.op_name}\"{version}>"

    def __eq__(self, other: Any):
        return (
            isinstance(other, OpRef)
            and other.op_ns == self.op_ns
            and other.op_name == self.op_name
            and other.op_version == self.op_version
        )


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

    def get_finalize(self) -> CmdArgs | None:
        return self._data.get("finalize")


ProgressSpec = str


class OpDefProgress:
    def __init__(self, data: Data):
        self._data = data

    def as_json(self) -> Data:
        return self._data

    def get_stage_sourcecode(self) -> ProgressSpec | None:
        return self._data.get("stage-sourcecode")

    def get_stage_dependencies(self) -> ProgressSpec | None:
        return self._data.get("stage-dependencies")

    def get_stage_runtime(self) -> ProgressSpec | None:
        return self._data.get("stage-runtime")

    def get_run(self) -> ProgressSpec | None:
        return self._data.get("run")

    def get_finalize(self) -> ProgressSpec | None:
        return self._data.get("finalize")


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


class OpDefSummary:
    def __init__(self, data: Data):
        self._data = data

    def as_json(self) -> Data:
        return self._data

    def get_filename(self):
        return self._data.get("filename")


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

    def get_version(self) -> str | None:
        val = self._data.get("version")
        return str(val) if val is not None else None

    def get_description(self) -> str | None:
        return self._data.get("description")

    def get_default(self):
        return bool(self._data.get("default"))

    def get_exec(self) -> OpDefExec:
        val = self._data.get("exec", {})
        if isinstance(val, str) or isinstance(val, list):
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
        elif all(isinstance(item, str) for item in val):
            val = [{"keys": val}]
        else:
            val = [{"keys": item} if isinstance(item, str) else item for item in val]
        return [OpDefConfig(item) for item in val]

    def get_dependencies(self) -> list[OpDefDependency]:
        val = self._data.get("depends")
        if val is None:
            val = []
        elif isinstance(val, dict):
            val = [val]
        return [OpDefDependency(item) for item in val]

    def get_summary(self) -> OpDefSummary:
        val = self._data.get("summary")
        if val is None:
            val = {}
        return OpDefSummary(val)

    def get_progress(self) -> OpDefProgress:
        val = self._data.get("progress", {})
        if isinstance(val, str) or isinstance(val, list):
            val = {"run": val}
        return OpDefProgress(val)

    def get_output_summary_pattern(self) -> str | bool | None:
        return self._data.get("output-summary")


class GageFile:
    def __init__(self, filename: str, data: Data):
        self.filename = filename
        self._data = data

    def as_json(self) -> Data:
        return self._data

    def get_runs_dir(self) -> str | None:
        return self._data.get("$runs-dir")

    def get_archives_dir(self) -> str | None:
        return self._data.get("$archives-dir")

    def get_operations(self):
        return {
            name: OpDef(name, self._data[name], self.filename)  # \
            for name in self._data
            if name[:1] != "$"
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

IndexedRun = tuple[int, Run]

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


class RunSummary:
    def __init__(self, data: dict[str, Any]):
        self._data = data

    def get_attributes(self) -> dict[str, Any]:
        return cast(dict[str, Any], self._data.get("attributes") or {})

    def get_metrics(self) -> dict[str, Any]:
        return cast(dict[str, Any], self._data.get("metrics") or {})

    def get_run_attrs(self) -> dict[str, Any]:
        return cast(dict[str, Any], self._data.get("run") or {})

    def as_json(self):
        return self._data


class RunProxy(Protocol):
    def __getitem__(self, name: str) -> Any: ...

    def get(self, name: str, default: Any = None) -> Any: ...


# =================================================================
# Runs list
# =================================================================


class RunFilter(Protocol):
    def __call__(self, run: Run) -> bool: ...


# =================================================================
# Board
# =================================================================

BoardDefColumn = dict[str, Any]


class BoardDefRunSelect:
    def __init__(self, data: dict[str, Any]):
        self._data = data

    def get_operation(self) -> str | None:
        return self._data.get("operation")

    def get_status(self) -> str | list[str] | None:
        return self._data.get("status")

    def get_group(self) -> dict[str, Any] | None:
        return self._data.get("from-group")


class BoardDefGroupSelect:
    def __init__(self, data: dict[str, Any]):
        self._data = data

    def get_group_by(self) -> dict[str, Any]:
        return self._data.get("group-by") or {}

    def get_min(self) -> dict[str, Any] | None:
        return self._data.get("min")

    def get_max(self) -> dict[str, Any] | None:
        return self._data.get("max")


class BoardDef:
    def __init__(self, filename: str, data: dict[str, Any]):
        self.filename = filename
        self._data = data

    def as_json(self):
        return self._data

    def get_id(self) -> str | None:
        return cast(str, self._data.get("id"))

    def get_name(self) -> str | None:
        return cast(str, self._data.get("name"))

    def get_title(self) -> str | None:
        return cast(str, self._data.get("title"))

    def get_description(self) -> str | None:
        return cast(str, self._data.get("description"))

    def get_group_column(self) -> BoardDefColumn | None:
        group_column = self._data.get("group-column")
        if group_column is None:
            return None
        return cast(BoardDefColumn, group_column)

    def get_columns(self) -> list[BoardDefColumn]:
        return [_coerce_col(col) for col in self._data.get("columns") or []]

    def get_run_select(self) -> BoardDefRunSelect | None:
        run_select = self._data.get("run-select")
        if run_select is None:
            return None
        return BoardDefRunSelect(run_select)

    def get_group_select(self) -> BoardDefGroupSelect | None:
        group_select = self._data.get("group-select")
        if group_select is None:
            return None
        return BoardDefGroupSelect(group_select)


def _coerce_col(col: Any) -> dict[str, Any]:
    return col if isinstance(col, dict) else {"field": str(col)}


# =================================================================
# Misc
# =================================================================


class ArchiveDef:
    def __init__(self, filename: str, data: dict[str, Any]):
        self.filename = filename
        self._data = data

    def get_id(self):
        return cast(str, self._data.get("id"))

    def get_name(self):
        return cast(str, self._data.get("name"))

    def get_last_archived(self):
        attr: Any = self._data.get("date")
        return cast(int, attr) if attr else None

    def as_json(self):
        return self._data

    def __repr__(self):
        return f"<ArchiveRef id=\"{self.get_id()}\" name=\"{self.get_name()}\">"


# =================================================================
# Misc
# =================================================================


UnifiedDiff = list[str]

JSONCompatible = None | bool | int | float | str | Sequence[Any] | Mapping[str, Any]
