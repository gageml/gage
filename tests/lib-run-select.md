# Run select

    >>> from gage._internal.run_select import *
    >>> from gage._internal.types import *

## Selecting runs

`select_runs()` returns a sub-list of runs given zero or more select
specs.

    >>> select_runs([], [])
    []

Runs are selected using one of three schemes:

- Run index (1 based)
- Run slice (1 based)
- Run ID or name

Create some runs.

    >>> runs = [
    ...     Run("a", OpRef("", ""), "", "", "A"),
    ...     Run("b", OpRef("", ""), "", "", "B"),
    ...     Run("c", OpRef("", ""), "", "", "C"),
    ...     Run("dd", OpRef("", ""), "", "", "DD"),
    ...     Run("de", OpRef("", ""), "", "", "DE"),
    ...     Run("9", OpRef("", ""), "", "", "nine"),
    ... ]


Index based scheme:

    >>> select_runs(runs, ["1"])
    [<Run id="a" name="A">]

    >>> select_runs(runs, ["2"])
    [<Run id="b" name="B">]

    >>> select_runs(runs, ["2", "3"])
    [<Run id="b" name="B">, <Run id="c" name="C">]

If an index is out of range, it's treated as an ID.

    >>> select_runs(runs, ["9"])
    [<Run id="9" name="nine">]

Slice based scheme:

    >>> select_runs(runs, [":"])  # +pprint
    [<Run id="a" name="A">,
     <Run id="b" name="B">,
     <Run id="c" name="C">,
     <Run id="dd" name="DD">,
     <Run id="de" name="DE">,
     <Run id="9" name="nine">]

    >>> select_runs(runs, [":1"])
    [<Run id="a" name="A">]

    >>> select_runs(runs, ["6:"])
    [<Run id="9" name="nine">]

    >>> select_runs(runs, ["0:2"])
    [<Run id="a" name="A">, <Run id="b" name="B">]

    >>> select_runs(runs, ["2:4"])
    [<Run id="b" name="B">, <Run id="c" name="C">, <Run id="dd" name="DD">]

    >>> select_runs(runs, ["-2:"])
    [<Run id="de" name="DE">, <Run id="9" name="nine">]

    >>> select_runs(runs, [":-4"])
    [<Run id="a" name="A">, <Run id="b" name="B">]

    >>> select_runs(runs, ["2:-2"])
    [<Run id="b" name="B">, <Run id="c" name="C">, <Run id="dd" name="DD">]

    >>> select_runs(runs, ["-2:6"])
    [<Run id="de" name="DE">, <Run id="9" name="nine">]

    >>> select_runs(runs, ["-3:-1"])
    [<Run id="dd" name="DD">, <Run id="de" name="DE">]

Run ID or name based:

    >>> select_runs(runs, ["a"])
    [<Run id="a" name="A">]

    >>> select_runs(runs, ["A"])
    [<Run id="a" name="A">]

    >>> select_runs(runs, ["a", "B"])
    [<Run id="a" name="A">, <Run id="b" name="B">]

ID and name specs are prefixes.

    >>> select_runs(runs, ["d"])
    [<Run id="dd" name="DD">, <Run id="de" name="DE">]

    >>> select_runs(runs, ["D"])
    [<Run id="dd" name="DD">, <Run id="de" name="DE">]

    >>> select_runs(runs, ["d", "D"])
    [<Run id="dd" name="DD">, <Run id="de" name="DE">]

    >>> select_runs(runs, ["ni"])
    [<Run id="9" name="nine">]

Runs are returned in their original order, regardless of the order of
select specs.

    >>> select_runs(runs, ["1", "2"])
    [<Run id="a" name="A">, <Run id="b" name="B">]

    >>> select_runs(runs, ["2", "1"])
    [<Run id="a" name="A">, <Run id="b" name="B">]

Spec schemes may be combined.

    >>> select_runs(runs, ["n", "a", "1"])
    [<Run id="a" name="A">, <Run id="9" name="nine">]
