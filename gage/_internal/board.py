# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import os

from .project_util import load_project_data
from .schema_util import validate_data

__all__ = [
    "default_board_config_path_for_dir",
    "load_board_data",
    "load_board_def",
    "validate_board_data",
]

DEFAULT_BOARD_CONFIG_NAMES = [
    # Specified in order of precedence
    "board.json",
    "board.toml",
    "board.yaml",
]


def validate_board_data(obj: JSONCompatible):
    result = validate_data(obj, "board")
    if not result.valid:
        raise BoardConfigValidationError(result)


def load_board_def(filename: str):
    data = load_project_data(filename)
    if not isinstance(data, dict):
        raise ValueError("expected a map")
    return BoardDef(filename, data)


def load_board_data(filename: str):
    try:
        return load_project_data(filename)
    except Exception as e:
        raise BoardConfigLoadError(filename, str(e))


def default_board_config_path_for_dir(dirname: str):
    for name in DEFAULT_BOARD_CONFIG_NAMES:
        path = os.path.join(dirname, name)
        if not os.path.exists(path):
            continue
        return path
    raise FileNotFoundError(dirname)
