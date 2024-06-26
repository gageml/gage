# SPDX-License-Identifier: Apache-2.0

from typing import *

from .types import *

from subprocess import Popen

import io
import logging
import struct
import threading
import time

__all__ = [
    "OutputCallback",
    "Progress",
    "ProgressParser",
    "RunOutputWriter",
    "RunOutputReader",
    "stream_fileno",
]

log = logging.getLogger(__name__)

RUN_OUTPUT_STREAM_BUFFER = 4096


def stream_fileno(stream: IO[bytes]):
    try:
        return stream.fileno()
    except AttributeError:
        return None
    except io.UnsupportedOperation:
        return None


StreamType = Literal[0, 1]


class Progress(NamedTuple):
    completed: float


ProgressParser: TypeAlias = Callable[[bytes], tuple[bytes, Progress | None]]


class OutputCallback(Protocol):
    def output(
        self,
        stream: StreamType,
        out: bytes,
        progress: Progress | None = None,
    ) -> None:
        raise NotImplementedError()

    def close(self) -> None:
        raise NotImplementedError()


class RunOutputWriter:
    def __init__(
        self,
        filename: str,
        output_cb: OutputCallback | None = None,
        progress_parser: ProgressParser | None = None,
    ):
        """Creates a run output object.

        Run output, written to a proc stdout and stderr files, is
        written to `filename`. An index of output lines is written to
        `filename` + ".index".

        Run output is not automatically opened. Use `open(proc)` to open
        output for a process.
        """
        self._filename = filename
        self._output_cb = output_cb
        self._progress_parser = progress_parser
        self._output_lock = threading.Lock()
        self._open = False
        self._proc = None
        self._output = None
        self._index = None
        self._out_tee = None
        self._err_tee = None

    @property
    def closed(self):
        return not self._open

    def open(self, proc: Popen[bytes]):
        """Opens output.

        When open, threads are started for reading from proc.stdout and
        proc.stderr and writing to sys.stdout and sys.stderr
        respectively.

        Generates an error if run output is closed.
        """
        self._assert_closed()
        if proc.stdout is None:
            raise RuntimeError("proc stdout must be a PIPE")
        self._proc = proc
        self._output = open(self._filename, "wb")
        self._index = open(self._filename + ".index", "wb")
        self._out_tee = threading.Thread(target=self._out_tee_run)
        self._out_tee.start()
        if proc.stderr:
            self._err_tee = threading.Thread(target=self._err_tee_run)
            self._err_tee.start()
        self._open = True

    def _assert_closed(self):
        assert not self._open
        assert self._proc is None
        assert self._output is None
        assert self._index is None
        assert self._out_tee is None
        assert self._err_tee is None

    def _out_tee_run(self):
        assert self._proc
        assert self._proc.stdout
        self._gen_tee_run(self._proc.stdout, 0)

    def _err_tee_run(self):
        assert self._proc
        assert self._proc.stderr
        self._gen_tee_run(self._proc.stderr, 1)

    def _gen_tee_run(self, input_stream: IO[bytes], stream_type: StreamType):
        assert self._output
        assert self._index
        while line := input_stream.readline():
            line_bytes, progress = self._process_line(line)
            with self._output_lock:
                self._output.write(line_bytes)
                index_entry = struct.pack("!QB", time.time_ns() // 1000000, stream_type)
                self._index.write(index_entry)
            self._apply_output_cb(stream_type, line_bytes, progress)

    def _process_line(self, line: bytes):
        if not self._progress_parser:
            return line, None
        try:
            return self._progress_parser(line)
        except Exception:
            log.exception("error in output callback (will be removed)")
            self._progress_parser = None
            return line, None

    def _apply_output_cb(
        self, stream_type: StreamType, output: bytes, progress: Any | None
    ):
        if not self._output_cb:
            return
        try:
            self._output_cb.output(stream_type, output, progress)
        except Exception:
            log.exception("error in output callback (will be removed)")
            self._output_cb = None
            return output, None

    def wait(self, timeout: float | None = None):
        """Wait for run output reader threads to exit.

        This call will block until the reader threads exit. Reader
        threads exit when the underlying streams they read from are
        closed. If these streams do not close, this call will not
        return. Streams close when their associated OS process
        terminates or they're otherwise explicitly closed.
        """
        self._assert_open()
        assert self._out_tee
        self._out_tee.join(timeout)
        if self._err_tee:
            self._err_tee.join(timeout)

    def _assert_open(self):
        if not self._open:
            raise RuntimeError("not open")
        assert self._proc
        assert self._output
        assert self._index
        assert self._out_tee
        assert not self._proc.stderr or self._err_tee

    def close(self):
        if not self._output_lock.acquire(timeout=60):
            raise RuntimeError("timeout")
        try:
            self._close()
        finally:
            self._output_lock.release()

    def _close(self):
        self._assert_open()
        assert self._output
        assert self._index
        self._output.close()
        self._index.close()
        if self._output_cb:
            try:
                self._output_cb.close()
            except Exception:
                log.exception("closing output callback")
        assert self._out_tee
        assert not self._out_tee.is_alive()
        assert not self._err_tee or not self._err_tee.is_alive()
        self._proc = None
        self._output = None
        self._index = None
        self._out_tee = None
        self._err_tee = None
        self._open = False

    def wait_and_close(self, timeout: float | None = None):
        self.wait(timeout)
        self.close()


class RunOutputLine(NamedTuple):
    timestamp: float
    stream: Literal[0, 1]
    text: str


class RunOutputReader:
    def __init__(self, name: str, output: IO[bytes], index: IO[bytes]):
        self.name = name
        self._output = output
        self._index = index
        self._lines: list[RunOutputLine] = []

    def __enter__(self):
        return self

    def __exit__(self, *exc: Any):
        self.close()

    def __iter__(self):
        cur = 0
        while True:
            self._read_next(cur)
            if len(self._lines) == cur:
                break
            yield self._lines[cur]
            cur += 1

    def read(self, start: int = 0, end: int | None = None) -> list[RunOutputLine]:
        """Read run output from start to end.

        Both `start` and `end` are zero-based indexes to run output
        lines and are both inclusive. Note this is different from the
        Python slice function where end is exclusive.
        """
        self._read_next(end)
        if end is None:
            slice_end = None
        else:
            slice_end = end + 1
        return self._lines[start:slice_end]

    def _read_next(self, end: int | None):
        if end is not None and end < len(self._lines):
            return
        while True:
            line_encoded = self._output.readline()
            if not line_encoded:
                break
            line = line_encoded.rstrip().decode()
            header = self._index.read(9)
            if len(header) < 9:
                break
            time, stream = struct.unpack("!QB", header)
            self._lines.append(RunOutputLine(time, stream, line))
            if end is not None and end < len(self._lines):
                break

    def close(self):
        _try_close(self._output)
        _try_close(self._index)


def _try_close(f: IO[bytes]):
    try:
        f.close()
    except IOError:
        pass
