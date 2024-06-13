---
test-options: +skip=WINDOWS_FIX (file locking)
---

# Progress example

    >>> use_example("progress")

    >>> run("gage check .")  # +paths
    ./gage.toml is a valid Gage file
    <0>

    >>> run("gage ops")
    | operation | description                   |
    |-----------|-------------------------------|
    | progress  | Shows progress for run phases |
    | tqdm      | Show tqdm based progress      |
    <0>

The `tqdm` example uses the `tqdm` module to show progress. Gage parses
the progress updates to prevent them from appearing in the run output.

    >>> run("gage run tqdm -y")  # +parse
    Collecting tqdm
    {}
    Installing collected packages: tqdm
    Successfully installed tqdm-{}
    Doing stuff 1
    Doing stuff 2
    Doing stuff 3
    Doing stuff 4
    Doing stuff 5
    Doing stuff 6
    Doing stuff 7
    Doing stuff 8
    Doing stuff 9
    Doing stuff 10
    <0>

    >>> run("gage select --meta-dir")  # +parse
    {meta_dir:path}
    <0>

    >>> run("gage show --output")
    Doing stuff 1
    Doing stuff 2
    Doing stuff 3
    Doing stuff 4
    Doing stuff 5
    Doing stuff 6
    Doing stuff 7
    Doing stuff 8
    Doing stuff 9
    Doing stuff 10
    <0>
