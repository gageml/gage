# SPDX-License-Identifier: Apache-2.0

from typing import *

from gage._internal.file_util import safe_delete_tree

from ..types import *

import json
import logging
import os
import subprocess
import tempfile

from urllib.parse import urlparse

import requests

from rich import prompt

from .. import cli

from .board_impl import Args as BoardArgs
from .board_impl import _load_board_def
from .board_impl import _board_runs
from .board_impl import _board_data

from .copy_impl import _copy_to_
from .copy_impl import _rclone_cmd


log = logging.getLogger()

DEFAULT_GAGE_API = "https://beta.gage.live/api"


class Args(NamedTuple):
    board_id: str
    runs: list[str]
    where: str
    config: str
    no_config: bool
    skip_runs: bool
    yes: bool


class BoardDest(NamedTuple):
    endpoint: str
    bucket: str
    access_key_id: str
    secret_access_key: str


def publish(args: Args):
    board_args = BoardArgs(
        args.runs,
        args.where,
        args.config,
        args.no_config,
        True,
        False,
    )
    board = _load_board_def(board_args)
    board_id = _board_id(args, board)
    token = _api_token()
    with cli.status() as status:
        status.update("Resolving board details")
        board_dest = _board_dest(board_id, token)
    runs = _board_runs(board, board_args)
    endpoint = _board_endpoint()
    _user_confirm_publish(args, board, board_id, runs, endpoint)
    with cli.status() as status:
        status.update("Publishing board data")
        rclone_conf, rclone_env = _publish_board_data(board, runs, board_dest, status)
    if not args.skip_runs:
        _copy_runs(runs, rclone_conf, rclone_env, board_dest)
    _delete_conf_tmp(rclone_conf)
    cli.out(f"View board at {endpoint}/boards/{board_id}")


def _board_endpoint():
    parsed = urlparse(_api_endpoint())
    return f"{parsed.scheme}://{parsed.netloc}"


def _board_id(args: Args, board: BoardDef):
    id = args.board_id or board.get_id()
    if not id:
        if args.config:
            cli.exit_with_error(
                f"Missing board ID: specify an 'id' attribute in {args.config} "
                "or use --board-id"
            )
        else:
            cli.exit_with_error(
                "Missing board ID: use '--config <path>' to specify a "
                "board config or use '--board-id <id>'"
            )
    return id


def _api_token():
    token = _token_for_env()
    if not token:
        token = _prompt_for_token()
        if not token:
            cli.exit_with_error(
                "Missing API token: specify GAGE_TOKEN environment variable"
            )
    if not token.startswith("gage_"):
        cli.exit_with_error("Invalid API token")
    return token


def _token_for_env():
    return os.getenv("GAGE_TOKEN") or _dot_env().get("GAGE_TOKEN")


def _dot_env() -> dict[str, str]:
    try:
        s = open(".env").read()
    except OSError:
        return {}
    else:
        return _parse_env(s)


def _parse_env(s: str):
    return dict([line.split("=", 1) for line in s.split("\n") if "=" in line])


def _prompt_for_token():
    console = prompt.get_console()
    if not console.is_interactive:
        return None
    return prompt.Prompt.ask("Enter your Gage API token", password=True)


def _user_confirm_publish(
    args: Args, board: BoardDef, board_id: str, runs: list[Run], endpoint: str
):
    if args.yes:
        return
    board_desc = board.get_name() or f"board {board_id}"
    endpoint_desc = _endpoint_desc(endpoint)
    skip_suffix = " (runs are not copied)" if args.skip_runs else ""
    cli.err(
        f"You are about to publish {len(runs):,} {'run' if len(runs) == 1 else 'runs'} "
        f"to {board_desc} on {endpoint_desc}{skip_suffix}"
    )
    cli.err()
    if not cli.confirm(f"Continue?"):
        raise SystemExit(0)


def _endpoint_desc(endpoint: str):
    parsed = urlparse(endpoint)
    return parsed.netloc


def _board_dest(board_id: str, token: str):
    endpoint = _api_endpoint()
    url = f"{endpoint}/v1/boards/{board_id}/creds"
    headers = {"Authorization": f"Bearer {token}"}
    log.debug("Getting board credentials at %s", url)
    try:
        resp = requests.get(url, headers=headers)
    except requests.RequestException as e:
        cli.exit_with_error(f"Error getting board information: {e}")
    else:
        if resp.status_code != 200:
            cli.exit_with_error(
                f"Error getting board information: {resp.content.decode()}"
            )
        data = json.loads(resp.content)
        board_dest = BoardDest(
            data["endpoint"],
            data["bucket"],
            data["accessKeyId"],
            data["secretAccessKey"],
        )
        log.info("Publishing to %s/%s", board_dest.endpoint, board_dest.bucket)
        return board_dest


def _api_endpoint():
    return os.getenv("GAGE_API") or _dot_env().get("GAGE_API") or DEFAULT_GAGE_API


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
    cmd = _rclone_cmd(
        [
            "--config",
            "rclone.conf",
            "copyto",
            "data.json",
            f"gage:{board_dest.bucket}/board.json",
        ]
    )
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
