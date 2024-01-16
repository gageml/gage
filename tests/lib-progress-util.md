# Progress Utils

The `progress_util` module provides progress-related support.

    >>> from gage._internal.progress_util import *

Progress is inferred from command output. Gage currently only supports
progress parsing for `tqdm`. Use `progress_parser` to return a function
to parse output.

    >>> progress_parser("tqdm")  # +wildcard
    <function _parse_tqdm at ...>

    >>> progress_parser("not-supported")
    Traceback (most recent call last):
    ValueError: not-supported

## tqdm

[tqdm](https://tqdm.github.io/) is a popular library for presenting
progress to users. Gage supports tqdm output as a configurable operation
option.

    >>> parse = progress_parser("tqdm")

Progress parsing in Gage is a line based scheme. Define sample output
lines that reflect tqdm scheme.

    >>> sample_lines = [
    ... b'\r',
    ... b'  0%|          | 0/10 [00:00<?, ?it/s]\r',
    ... b'                                      \r',
    ... b'Doing stuff 1\n',
    ... b'\r',
    ... b'  0%|          | 0/10 [00:00<?, ?it/s]\r',
    ... b' 10%|\xe2\x96\x88         | 1/10 [00:00<00:00,  9.97it/s]\r',
    ... b'                                              \r',
    ... b'Doing stuff 2\n',
    ... b'\r',
    ... b' 10%|\xe2\x96\x88         | 1/10 [00:00<00:00,  9.97it/s]\r',
    ... b' 20%|\xe2\x96\x88\xe2\x96\x88        | 2/10 [00:00<00:00,  9.96it/s]\r',
    ... b'                                              \r',
    ... b'Doing stuff 3\n',
    ... ]

Parse the sample lines.

    >>> for line in sample_lines:
    ...     print(parse(line))
    (b'', None)
    (b'', Progress(completed=0))
    (b'', None)
    (b'Doing stuff 1\n', None)
    (b'', None)
    (b'', Progress(completed=0))
    (b'', Progress(completed=10))
    (b'', None)
    (b'Doing stuff 2\n', None)
    (b'', None)
    (b'', Progress(completed=10))
    (b'', Progress(completed=20))
    (b'', None)
    (b'Doing stuff 3\n', None)
