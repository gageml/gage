# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

import os

from .attr_log import make_log_id
from .attr_log import now_ms
from .attr_log import get_attrs_by_author
from .attr_log import log_attrs

from .run_util import run_user_dir

from .file_util import ensure_dir

from .sys_config import get_user

__all__ = [
    "add_comment",
    "delete_comment",
    "set_comment",
    "get_comments",
]


def _make_comment_id():
    timestamp = now_ms()
    return make_log_id(timestamp)[:13], timestamp


def add_comment(run: Run, msg: str):
    comment_id, timestamp = _make_comment_id()
    attrs = {f"comment:{comment_id}": {"msg": msg, "timestamp": timestamp}}
    attrs_dir = run_user_dir(run)
    ensure_dir(attrs_dir)
    log_attrs(attrs_dir, get_user(), attrs)
    return comment_id


def get_comments(run: Run) -> list[RunComment]:
    attrs_dir = run_user_dir(run)
    if not os.path.exists(attrs_dir):
        return []
    comments = []
    for author, attrs in get_attrs_by_author(attrs_dir).items():
        for name in attrs:
            if name.startswith("comment:"):
                comment_id = name[8:]
                comment_attrs = attrs[name]
                comments.append(
                    RunComment(
                        comment_id,
                        author,
                        comment_attrs["timestamp"],
                        comment_attrs["msg"],
                    )
                )
    comments.sort(key=lambda c: c.timestamp)
    return comments


def set_comment(run: Run, comment_id: str, msg: str):
    timestamp = now_ms()
    attrs = {f"comment:{comment_id}": {"msg": msg, "timestamp": timestamp}}
    attrs_dir = run_user_dir(run)
    ensure_dir(attrs_dir)
    log_attrs(attrs_dir, get_user(), attrs)


def delete_comment(run: Run, comment_id: str):
    attrs_dir = run_user_dir(run)
    if not os.path.exists(attrs_dir):
        return
    log_attrs(attrs_dir, get_user(), {}, [f"comment:{comment_id}"])
