# SPDX-License-Identifier: Apache-2.0

from typing import *

from gage._internal.file_util import safe_delete_tree

from ..types import *

import json
import logging
import os
import subprocess
import tempfile

import requests

from .. import cli

from .board_impl import Args as BoardArgs
from .board_impl import _init_board
from .board_impl import _board_runs
from .board_impl import _board_data

from .copy_impl import _copy_to_


log = logging.getLogger()

DEFAULT_GAGE_API = "https://beta.gage.live/api"


class Args(NamedTuple):
    board: str
    runs: list[str]
    where: str
    config: str
    skip_runs: bool
    yes: bool


class BoardDest(NamedTuple):
    endpoint: str
    bucket: str
    access_key_id: str
    secret_access_key: str


def publish(args: Args):
    board_args = BoardArgs(args.runs, args.where, args.config, True, False)
    board = _init_board(board_args)
    board_id = _board_id(args, board)
    runs = _board_runs(board, board_args)
    with cli.status() as status:
        status.update("Resolving board details")
        board_dest = _board_dest(board_id)
    _user_confirm_publish(args, board, board_id, runs)
    with cli.status() as status:
        status.update("Publishing board data")
        rclone_conf, rclone_env = _publish_board_data(board, runs, board_dest, status)
    if not args.skip_runs:
        _copy_runs(runs, rclone_conf, rclone_env, board_dest)
    _delete_conf_tmp(rclone_conf)


def _board_id(args: Args, board: BoardDef):
    id = args.board or board.get_id()
    if not id:
        if args.config:
            cli.exit_with_error(
                "Missing board ID: use '--board <id>' or specify an 'id' "
                f"attribute in {args.config}"
            )
        else:
            cli.exit_with_error(
                "Missing board ID: use '--board <id>' to specify the board to publish"
            )
    return id


def _user_confirm_publish(args: Args, board: BoardDef, board_id: str, runs: list[Run]):
    if args.yes:
        return
    board_desc = board.get_name() or f"board {board_id}"
    skip_suffix = " (runs are not copied)" if args.skip_runs else ""
    cli.err(
        f"You are about to publish {len(runs):,} {'run' if len(runs) == 1 else 'runs'} "
        f"to {board_desc} on Gage Live{skip_suffix}"
    )
    cli.err()
    if not cli.confirm(f"Continue?"):
        raise SystemExit(0)


def _board_dest(board_id: str):
    token = os.getenv("GAGE_TOKEN")
    if not token:
        cli.exit_with_error(
            "Missing API token: specify GAGE_TOKEN environment variable"
        )
    endpoint = os.getenv("GAGE_API") or DEFAULT_GAGE_API
    url = f"{endpoint}/v1/boards/{board_id}/creds"
    headers = {"Authorization": f"Bearer {token}"}
    log.debug("Getting board credentials at %s", url)
    try:
        resp = requests.get(url, headers=headers)
    except requests.HTTPError as e:
        cli.exit_with_error(f"Error publishing to {endpoint}: {e}")
    else:
        data = json.loads(resp.content)
        board_dest = BoardDest(
            data["endpoint"],
            data["bucket"],
            data["accessKeyId"],
            data["secretAccessKey"],
        )
        log.info("Publishing to %s/%s", board_dest.endpoint, board_dest.bucket)
        return board_dest


RCLONE_CONF = """
[gage]
type = s3
provider = Cloudflare
endpoint = {endpoint}
acl = private
no_check_bucket = true
"""


def _publish_board_data(
    board: BoardDef, runs: list[Run], board_dest: BoardDest, status: cli.Status
):
    data = _board_data(board, runs)
    tmp = tempfile.mkdtemp(prefix="gage-publish-")
    log.debug("Using %s for rclone config", tmp)
    data_path = os.path.join(tmp, "data.json")
    with open(data_path, "w") as f:
        json.dump(data, f)
    conf_path = os.path.join(tmp, "rclone.conf")
    with open(conf_path, "w") as f:
        f.write(RCLONE_CONF.format(endpoint=board_dest.endpoint))
    cmd = [
        "rclone",
        "--config",
        "rclone.conf",
        "copyto",
        "data.json",
        f"gage:{board_dest.bucket}/board.json",
    ]
    conf_env = {
        "RCLONE_CONFIG_GAGE_ACCESS_KEY_ID": board_dest.access_key_id,
        "RCLONE_CONFIG_GAGE_SECRET_ACCESS_KEY": board_dest.secret_access_key,
    }
    try:
        subprocess.run(
            cmd,
            env=conf_env,
            cwd=tmp,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=True,
        )
    except subprocess.CalledProcessError as e:
        status.stop()
        log.error(e.output)
        raise SystemExit(e.returncode)
    else:
        return conf_path, conf_env


def _copy_runs(
    runs: list[Run], conf_path: str, conf_env: dict[str, str], board_dest: BoardDest
):
    dest = f"gage:{board_dest.bucket}/runs"
    _copy_to_(runs, dest, sync=True, config_path=conf_path, env=conf_env)


def _delete_conf_tmp(conf_path: str):
    tmp_root = tempfile.gettempdir()
    conf_tmp = os.path.dirname(conf_path)
    assert conf_tmp.startswith(tmp_root), (conf_path, tmp_root)
    safe_delete_tree(conf_tmp)
