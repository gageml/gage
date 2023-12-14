# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import os

from . import sys_config

from .project_util import find_project_dir
from .project_util import load_data
from .schema_util import validate_data

__all__ = [
    "gagefile_candidates",
    "gagefile_path_for_dir",
    "load_gagefile",
    "load_gagefile_data",
    "validate_gagefile_data",
]

GAGEFILE_NAMES = [
    os.path.join(".gage", "gage.toml"),
    os.path.join(".gage", "gage.yaml"),
    os.path.join(".gage", "gage.json"),
    "gage.toml",
    "gage.yaml",
    "gage.json",
]


def validate_gagefile_data(obj: JSONCompatible):
    result = validate_data(obj, "gagefile")
    if not result.valid:
        raise GageFileValidationError(result)


def load_gagefile(filename: str):
    data = load_gagefile_data(filename)
    return GageFile(filename, data)


def load_gagefile_data(filename: str):
    try:
        return load_data(filename)
    except Exception as e:
        raise GageFileLoadError(filename, str(e))


def gagefile_for_dir(dirname: str):
    return load_gagefile(gagefile_path_for_dir(dirname))


def gagefile_path_for_dir(dirname: str):
    for name in gagefile_candidates():
        path = os.path.join(dirname, name)
        if not os.path.exists(path):
            continue
        return path
    raise FileNotFoundError(dirname)


def gagefile_candidates():
    return GAGEFILE_NAMES


def gagefile_for_project(cwd: str | None = None):
    cwd = cwd or sys_config.cwd()
    project_dir = find_project_dir(cwd)
    if not project_dir:
        raise FileNotFoundError(cwd)
    return gagefile_for_dir(project_dir)
