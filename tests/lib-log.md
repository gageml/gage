---
# Disable debug logging because we tamper with logging levels in
# these tests.
test-options: -debug
---

# Logging

Logging in Guild is performed using Python's built in `logging` module
and related facility.

    >>> import logging

The module `gage._internal.log` is used to initialize this facility.

    >>> from gage._internal import log as loglib

For our tests, we'll create a function that logs various messages
using a logger named `test`.

Here's our test logger:

    >>> test_logger = logging.getLogger("test")

and our logging function:

    >>> def log_sample_messages():
    ...     test_logger.debug("debug entry")
    ...     test_logger.info("info entry")
    ...     test_logger.warning("warning entry")
    ...     test_logger.error("error entry")

## Logging Scheme Rationale

By default, INFO and lower logging levels are not shown to the user. To
show these levels, the user must specify `-v/--verbose` or `-vv` to show
INFO and DEBUG levels respectively.

WARN levels and above are shown to users.

To ensure that a user sees a message, use `cli.out` or `cli.err`. These
are the designated user-facing interface functions. Calls to these
functions MUST only be made from command implementations. They MUST
NOT be called from anywhere else.

To show information from non-command code, use logging.

Use the following guidelines when choosing a log level:

Use `DEBUG` for low-level information that is used only when debugging
issues.

Use `INFO` to provide user-facing descriptions of what is occurring.
Think of `INFO` as progress status from the logging subsystem.

Use `WARN` to show error or unexpected state details when the system
continues to run.

Use `ERROR` to show information prior to exiting the system with an
error code.

Use `EXCEPTION` only when debug level logging is enabled to show
exception tracebacks. Use this pattern:

```python
if log.getEffectiveLevel() <= logging.DEBUG:
    log.exception("<Additional contexts for the error>")
    # Optionally perform additional tasks before exiting or re-raising
    raise  # or raise SystemExit(<error_code>)
```

When logging, always capitalize the first word, unless the word is a
term that cannot be capitalized. Do not end log messages with
punctuation. The intent is to present a user-facing message that is not
a full sentence.

## Initializing Logging

We use the `init_logging` function to initialize the logging
facility. First, let's save the current settings so we can restore
them at the end of our tests.

    >>> original_log_settings = loglib.current_settings()

Let's initialize logging with the default settings:

    >>> loglib.init_logging()

Debug is not logged by default:

    >>> from gage._internal.util import LogCapture

    >>> log_capture = LogCapture(use_root_handler=True)

    >>> with log_capture:
    ...     log_sample_messages()

    >>> log_capture.print_all()
    info entry
    WARNING: warning entry
    ERROR: error entry

## Enable Debug

Re-init with debug enabled.

    >>> loglib.init_logging(logging.DEBUG)

    >>> with log_capture:
    ...     log_sample_messages()

    >>> log_capture.print_all()
    DEBUG: [test] debug entry
    info entry
    WARNING: warning entry
    ERROR: error entry

## Color

If a TTY is available and the SHELL environment variable is defined,
output is colored. Warnings are displayed in yellow (color code 33)
and errors in red (color code 31).

    >>> with loglib._FakeTTY():
    ...   with loglib._FakeShell():
    ...      with LogCapture(use_root_handler=True,
    ...          strip_ansi_format=False
    ...      ) as tty_logs:
    ...          log_sample_messages()
    ...      tty_logs.print_all()
    DEBUG: [test] debug entry
    info entry
    [33mWARNING: warning entry[0m
    [31mERROR: error entry[0m

## Custom Formats

Specify custom level formats.

    >>> loglib.init_logging(
    ...   logging.DEBUG,
    ...   formats={"_": "%(levelno)i %(message)s"}
    ... )

    >>> with log_capture:
    ...   log_sample_messages()

    >>> log_capture.print_all()
    10 debug entry
    20 info entry
    30 warning entry
    40 error entry

Define the WARN and ERROR formats.

    >>> loglib.init_logging(
    ...   logging.DEBUG,
    ...   formats={"WARNING": "!! %(message)s",
    ...            "ERROR": "!!! %(message)s"}
    ... )

    >>> with log_capture:
    ...   log_sample_messages()

    >> log_capture.print_all()
    debug entry
    info entry
    !! warning entry
    !!! error entry

## Restoring Logging

Restore logging to its defaults.

    >> gage._internal.log.init_logging(**original_log_settings)
