# SPDX-License-Identifier: Apache-2.0

from typing import *

from ..types import *

import json
import logging
import os
import subprocess
import tempfile

from .. import cli

from .board_impl import Args as BoardArgs
from .board_impl import _init_board
from .board_impl import _board_runs
from .board_impl import _board_data

from .copy_impl import _copy_to_


log = logging.getLogger()


class Args(NamedTuple):
    board: str
    runs: list[str]
    where: str
    config: str
    yes: bool


class BoardDest(NamedTuple):
    endpoint: str
    bucket: str
    access_key_id: str
    secret_access_key: str


def publish(args: Args):
    board_args = BoardArgs(args.runs, args.where, args.config, True, False)
    board = _init_board(board_args)
    board_name = _board_name(args, board)
    runs = _board_runs(board, board_args)
    _user_confirm_publish(args, board_name, runs)
    with cli.status() as status:
        status.update("Resolving board details")
        board_dest = _board_dest(board_name)
        status.update("Publishing board data")
        rclone_conf = _publish_board_data(board, runs, board_dest, status)
    _copy_runs(runs, rclone_conf, board_dest)


def _board_name(args: Args, board: BoardDef):
    name = args.board or board.get_name()
    if not name:
        if args.config:
            cli.exit_with_error(
                "Unknown board name: use '--board' to specify the board "
                f"to publish or specify a name in {args.config}"
            )
        else:
            cli.exit_with_error(
                "Unknown board name: use '--board' to specify the board " "to publish"
            )
    return name


def _user_confirm_publish(args: Args, name: str, runs: list[Run]):
    if args.yes:
        return
    cli.err(
        f"You are about to publish {len(runs):,} {'run' if len(runs) == 1 else 'runs'} "
        f"to {name}"
    )
    cli.err()
    if not cli.confirm(f"Continue?"):
        raise SystemExit(0)


def _board_dest(board_name: str):
    import time

    time.sleep(1.25)

    # TODO - lookup this from API using something secure!

    return BoardDest(
        "https://d5f5a59ff84ba96f4eba7a056261fd17.r2.cloudflarestorage.com",
        "gage-boards",
        "b3d4bd0de70d2a583e257be5045f1043",
        "9ba1ee5bbe23b7c7ef346e864d38e786f5d8756b1586e8393995708e9d33b5d8",
    )


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
    env = {
        "RCLONE_CONFIG_GAGE_ACCESS_KEY_ID": board_dest.access_key_id,
        "RCLONE_CONFIG_GAGE_SECRET_ACCESS_KEY": board_dest.secret_access_key,
    }
    try:
        subprocess.run(
            cmd,
            env=env,
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
        return conf_path


def _copy_runs(runs: list[Run], rclone_conf_path: str, board_dest: BoardDest):
    dest = f"gage:{board_dest.bucket}/runs"
    _copy_to_(runs, dest, sync=True)
