---
test-options: +skip=WINDOWS_FIX
---

# `util purge-run-files` command

    >>> run("gage util purge-run-files -h")  # +diff
    Usage: gage util purge-run-files [options] [run]...
    ⤶
      Permanently delete run files.
    ⤶
      WARNING: This operation may break runs if it deletes
      critical files. Use when you're certain that the deleted
      files are no longer needed for the run. This is
      typically done to free disk space used by
      unintentionally generated files.
    ⤶
    Arguments:
      [run]...  Runs to select.
    ⤶
    Options:
      -w, --where expr    Select runs matching filter
                          expression.
      -a, --all           Purge match files for all runs.
      -f, --file pattern  File pattern using glob syntax.
      --preview           Show file select preview.
      -h, --help          Show this message and exit.
    <0>

Generate a run.

    >>> use_example("hello")

    >>> run("gage run -y")
    Hello Gage
    <0>

    >>> run("gage show --files -0")
    | name      | type        |
    |-----------|-------------|
    | gage.toml | source code |
    | hello.py  | source code |
    <0>

Attempt to purge files without specifying runs or `--all`.

    >>> run("gage util purge-run-files")
    gage: Specify a run or use '--all'.
    ⤶
    Try 'gage util purge-run-files -h' for additional help.
    <1>

Attempt to purge files without specifying a file pattern.

    >>> run("gage util purge-run-files --all")
    gage: Specify at least one file pattern using '-f / --files'
    <1>

Preview matching files.

    >>> run("gage util purge-run-files -a -f gage.toml --preview")
    ... # +table +parse
    | Run          | Path      | Matching Pattern |
    |--------------|-----------|------------------|
    | {x:run_name} | gage.toml | gage.toml        |
    | {y:run_name} | hello.py  | <no match>       |
    <0>

    >>> assert x == y

    >>> run(f"gage util purge-run-files {x} -f 'hello.*' --preview")
    ... # +table +parse
    | Run         | Path      | Matching Pattern |
    |-------------|-----------|------------------|
    | {y}         | gage.toml | <no match>       |
    | {z}         | hello.py  | hello.*         |
    <0>

    >>> assert x == y == z

    >>> run(f"gage util purge-run-files {x} -f 'hello.*' -f '?age.*' --preview")
    ... # +table +parse
    | Run         | Path      | Matching Pattern |
    |-------------|-----------|------------------|
    | {}          | gage.toml | ?age.*           |
    | {}          | hello.py  | hello.*          |
    <0>

Delete a run file. Use the hidden option `--confirm` to purge without
prompting.

    >>> run("gage util purge-run-files -a -f '*' --confirm 'I agree'")
    Deleted 2 file(s)
    <0>

    >>> run("gage show --files -0")
    | name | type |     |
    |------|------|-----|
    | ...  | ...  | ... |
    <0>

    >>> run("gage util purge-run-files -a -f '*' --preview")
    | Run | Path | Matching Pattern |
    |-----|------|------------------|
    <0>

Edge case: delete the run directory.

    >>> run("gage select --run-dir")  # +parse
    {run_dir:path}
    <0>

    >>> delete_tree(run_dir)

    >>> run("gage util purge-run-files -a -f '*' --preview")
    | Run | Path | Matching Pattern |
    |-----|------|------------------|
    <0>
