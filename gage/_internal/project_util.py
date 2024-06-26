# SPDX-License-Identifier: Apache-2.0

from typing import *

import os

from . import file_util
from . import sys_config

__all__ = [
    "find_project_dir",
    "has_project_marker",
    "load_project_data",
    "project_namespace",
]

PROJECT_MARKERS = [
    (".gage",),
    ("gage.json",),
    ("gage.toml",),
    ("gage.yaml",),
    ("gageconfig.json",),
    ("gageconfig.toml",),
    ("gageconfig.yaml",),
    ("pyproject.toml",),
    (".vscode",),
    (".git",),
]


def find_project_dir(dirname: str | None = None, stop_dir: str | None = None):
    explicit_dirname = os.getenv("PROJECT_DIR")
    if explicit_dirname:
        return explicit_dirname
    dirname = dirname or sys_config.cwd()
    last = None
    while True:
        if has_project_marker(dirname):
            return dirname
        if stop_dir and file_util.compare_paths(stop_dir, dirname):
            return None
        last = dirname
        dirname = os.path.dirname(dirname)
        if dirname == last:
            return None


def has_project_marker(dir: str):
    return any(os.path.exists(os.path.join(dir, *marker)) for marker in PROJECT_MARKERS)


def load_project_data(filename: str):
    if not os.path.exists(filename):
        raise FileNotFoundError(f"file does not exist: {filename}")
    ext = os.path.splitext(filename)[1].lower()
    if ext == ".toml":
        return _load_toml(filename)
    if ext == ".json":
        return _load_json(filename)
    if ext in (".yaml", ".yml"):
        return _load_yaml(filename)
    raise ValueError(f"unsupported file extension for {filename}")


def _load_toml(filename: str):
    import tomli

    with open(filename, "rb") as f:
        return tomli.load(f)


def _load_json(filename: str):
    import json

    with open(filename) as f:
        s = "".join([line for line in f if line.lstrip()[:2] != "//"])
        return json.loads(s)


def _load_yaml(filename: str):
    import yaml

    with open(filename) as f:
        return yaml.safe_load(f)


def project_namespace(cwd: str | None = None):
    project_dir = find_project_dir(cwd)
    return os.path.basename(project_dir) if project_dir else None
