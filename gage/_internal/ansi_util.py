# SPDX-License-Identifier: Apache-2.0

import re

_ansi_p = re.compile(r"\033\[[;?0-9]*[a-zA-Z]")


def strip_ansi(s: str):
    return _ansi_p.sub("", s)
