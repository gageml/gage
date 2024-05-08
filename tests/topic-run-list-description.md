# Run List Description

`gage runs` lists runs with a description column. The contents of the
column can be specified per operation using a "listing description"
spec.

The `listing-description` sample project illustrates this.

    >>> use_project(sample("projects", "listing-description"))

    >>> run("gage ops")
    | operation | description                                  |
    |-----------|----------------------------------------------|
    | op        | Specifies fields used in listing description |
    | op-2      | Same as run but uses default listing         |
    |           | description                                  |
    <0>

Generate some runs.

    >>> run("gage run op -y")
    <0>

    >>> run("gage run op s=bye -l 'saying buh bye' -y")
    <0>

    >>> run("gage run op-2 -y")
    <0>

    >>> run("gage run op-2 s=hi -l 'ho hi' -y")
    <0>

Show the runs. Note that `op` runs use fields `z` and `s`, which is the
ordered list of fields specified for the listing description in the op
def. `op-2` runs use the default field listing.

    >>> run("gage runs -s", cols=64)  # +table
    | # | operation | status    | description                      |
    |---|-----------|-----------|----------------------------------|
    | 1 | op-2      | completed | ho hi b=true i=123 s=hi z=1.123  |
    | 2 | op-2      | completed | b=true i=123 s=hello z=1.123     |
    | 3 | op        | completed | saying buh bye z=1.123 s=bye     |
    | 4 | op        | completed | z=1.123 s=hello                  |
    <0>
