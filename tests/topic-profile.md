# Profiling Gage

Gage profiles a command when the env var `PROFILE` is set to `1`.

    >>> run("gage -h", env={"PROFILE": "1"})  # +stderr +parse
    Profiling command
    Usage: gage [options] command
    ⤶
      Gage command line interface.
    ⤶
    {}
    Writing profile to {profile_filename:path}
    <0>

A profile stats file is written to `profile_filename`.

    >>> os.path.exists(profile_filename)
    True

Use `pstats` to view the profile contents.

    >>> import pstats, io

    >>> s = io.StringIO()
    >>> stats = pstats.Stats(profile_filename, stream=s)
    >>> _ = stats.print_stats(3)
    >>> print(s.getvalue())  # -space +parse
    {}
    ⤶
             {} function calls ({} primitive calls) in {} seconds
    ⤶
       Random listing order was used
       List reduced from {} to 3 due to restriction <3>
    ⤶
       ncalls  tottime  percall  cumtime  percall filename:lineno(function)
       {}
