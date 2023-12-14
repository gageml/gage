# SPDX-License-Identifier: Apache-2.0

from typing import *

# __all__ = [
#     "Event",
#     "Remote",
#     "RemoteError",
#     "remote_copy_to",
# ]


# class RemoteError(Exception):
#     pass


# class Remote:
#     pass


# class Event(NamedTuple):
#     output: str | None
#     progress_completed: float | int | None


# class Task(Protocol):
#     def init(self) -> None:
#         ...

#     def wait_for_event(self) -> None:
#         ...

#     def iter_events(self) -> Generator[Event, Any, None]:
#         ...

#     def get_init_desc(self) -> str:
#         ...

#     def get_waiting_for_event_desc(self) -> str:
#         ...

#     def get_task_desc(self) -> str:
#         ...

#     def get_progress_total(self) -> float | int | None:
#         ...

#     def get_progress_completed(self) -> float | int | None:
#         ...


# def remote_copy_to(dest: str) -> Task:
#     # TODO: parse dest for a remote prefix and look it up either in Gage
#     # config or in rclone config.
#     for remote_mod in remote_mods():
#         try:
#             remote_mod.remote_copy_to(dest)
#         except ValueError:
#             pass
#     raise RemoteError(f"Destination \"{dest}\" is not supported")


# def remote_mods():
#     from . import repo_rclone
#     from . import repo_git

#     yield repo_rclone
#     yield repo_git
