---
parse-types:
  timestamp: 1[6-7]\d{11}
---

# Run output

Run output is logged and read using facilities in
`gage._internal.run_output`.

    >>> from gage._internal.run_output import *

Output is written to two files: a plain text file containing merged
stderr and stdout output and an index, which annotates each logged line
with a timestamp and a stream time (err or out).

Create a sample script to generate output.

    >>> cd(make_temp_dir())

    >>> write("test.py", """
    ... import sys, time
    ...
    ... for i in range(5):
    ...     sys.stdout.write(f"stdout line {i}\\n")
    ... sys.stdout.flush()
    ... time.sleep(0.5)
    ...
    ... for i in range(5):
    ...     sys.stderr.write(f"stderr line {i}\\n")
    ... """)

Create a run output instance.

    >>> output = RunOutput("output")

Output is read from process output via `output.open()`. Create a process
to read from. To read both stdout and stderr, use pipes for each stream.

    >>> import subprocess

    >>> proc = subprocess.Popen(
    ...     [sys.executable, "test.py"],
    ...     stdout=subprocess.PIPE,
    ...     stderr=subprocess.PIPE,
    ... )

Prior to calling `open()` the output is considered closed.

    >>> output.closed
    True

Open run output with the process.

    >>> output.open(proc)

    >>> output.closed
    False

Process output is read using background threads.

Wait for the process to exit.

    >>> proc.wait()
    0

Wait for output to be written.

    >>> output.wait_and_close(timeout=1.0)

    >>> output.closed
    True

Two files are generated.

    >>> ls()
    output
    output.index
    test.py

The output log contains process output.

    >>> cat("output")
    stdout line 0
    stdout line 1
    stdout line 2
    stdout line 3
    stdout line 4
    stderr line 0
    stderr line 1
    stderr line 2
    stderr line 3
    stderr line 4

Note that output is not synchronized as might be expected. The test
script writes to stdout and stderr, which are separate streams and
appear as such in captured output.

To ensure that output is synchronized, the process should be configured
to use stdout for the stderr stream (see below).

While output can be read directly from `output_filename` as a text file,
a run output reader provides timestamp and stream type information for
each line.

    >>> with RunOutputReader("output") as reader:  # +parse
    ...     for timestamp, stream, line in reader:
    ...         print(timestamp, stream, line)
    {:timestamp} 0 stdout line 0
    {:timestamp} 0 stdout line 1
    {:timestamp} 0 stdout line 2
    {:timestamp} 0 stdout line 3
    {:timestamp} 0 stdout line 4
    {:timestamp} 1 stderr line 0
    {:timestamp} 1 stderr line 1
    {:timestamp} 1 stderr line 2
    {:timestamp} 1 stderr line 3
    {:timestamp} 1 stderr line 4

To synchronize stdout and stderr streams, open the process as follows:

    >>> proc = subprocess.Popen(
    ...     [sys.executable, "test.py"],
    ...     stdout=subprocess.PIPE,
    ...     stderr=subprocess.STDOUT,
    ... )

Open output for the process.

    >>> output.open(proc)

    >>> output.closed
    False

    >>> proc.wait()
    0

    >>> output.wait_and_close()

    >>> output.closed
    True

Show captured output.

    >>> cat("output")
    stdout line 0
    stdout line 1
    stdout line 2
    stdout line 3
    stdout line 4
    stderr line 0
    stderr line 1
    stderr line 2
    stderr line 3
    stderr line 4

The order is the same using an output reader.

    >>> with RunOutputReader("output") as reader:  # +parse
    ...     for timestamp, stream, line in reader:
    ...         print(timestamp, stream, line)
    {:timestamp} 0 stdout line 0
    {:timestamp} 0 stdout line 1
    {:timestamp} 0 stdout line 2
    {:timestamp} 0 stdout line 3
    {:timestamp} 0 stdout line 4
    {:timestamp} 0 stderr line 0
    {:timestamp} 0 stderr line 1
    {:timestamp} 0 stderr line 2
    {:timestamp} 0 stderr line 3
    {:timestamp} 0 stderr line 4

## Callback

Run output supports a callback interface, which can be used to respond
to output.

Create a sample program that generates output.

    >>> cd(make_temp_dir())

    >>> write("prog.py", """
    ... print("Line 1")
    ... print("Line 2")
    ... print("Line 3")
    ... """)

Create a callback to handle output.

    >>> class OutputHandler:
    ...     def output(self, stream, line):
    ...         print(f"Got output [{stream}]: {line}")
    ...
    ...     def close(self):
    ...         print("Handler close")

Create a run output object with a callback.

    >>> output = RunOutput("output", output_cb=OutputHandler())

    >>> p = subprocess.Popen(
    ...     [sys.executable, "prog.py"],
    ...     stdout=subprocess.PIPE,
    ...     stderr=subprocess.STDOUT
    ... )

    >>> output.open(p)
    >>> p.wait()
    Got output [0]: b'Line 1\n'
    Got output [0]: b'Line 2\n'
    Got output [0]: b'Line 3\n'
    0

    >>> output.wait_and_close()
    Handler close

    >>> cat("output")
    Line 1
    Line 2
    Line 3
