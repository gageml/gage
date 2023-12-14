# SPDX-License-Identifier: Apache-2.0

from typing import *

__all__ = [
    "Channel",
    "Listener",
]

Listener = Callable[[str, Any], None]


class Channel:
    def __init__(self):
        self._listeners = []

    def add(self, listener: Listener):
        self._listeners.append(listener)

    def remove(self, listener: Listener):
        self._listeners.remove(listener)

    def notify(self, name: str, arg: Any | None = None):
        for l in self._listeners:
            l(name, arg)
