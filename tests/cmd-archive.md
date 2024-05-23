# `archive` Command

The `archive` command moves runs out of the runs dir into an
archive-specific directory.

    >>> run("gage archive -h")  # +diff
    Usage: gage archive [options] [run]...
    ⤶
      Archive runs.
    ⤶
      By default, selected runs are moved to a new archive
      with a default name. Use '--name' to specify a different
      name. If an archive with a specified name exists,
      selected runs are added to that archive.
    ⤶
      Use '--list' to list archives.
    ⤶
      Use '--delete' to delete an archive. WARNING: Deleted
      archives cannot be recovered.
    ⤶
      Use '--rename' to rename an archive.
    ⤶
      To restore runs from an archive, use 'gage restore
      --archive <name>'. To view runs in an archive use 'gage
      runs --archive <name>'.
    ⤶
    Arguments:
      [run]...  Runs to archive. Required unless '--all' is
                specified.
    ⤶
    Options:
      -n, --name name           Use the specified archive
                                name.
      -d, --delete name         Delete the specified archive.
                                Use '--list' to show archives.
      -r, --rename current new  Rename the archive named
                                current to new. Use '--list'
                                show show comments.
      -l, --list                Show local archives.
      -w, --where expr          Archive runs matching filter
                                expression.
      -a, --all                 Archive all runs.
      -y, --yes                 Make changes without
                                prompting.
      -h, --help                Show this message and exit.
    <0>

## Archive Runs

Use `hello` to generate some runs.

    >>> use_example("hello")

    >>> run("gage run hello name=Moon -y")
    Hello Moon
    <0>

    >>> run("gage run hello name=Room -y")
    Hello Room
    <0>

    >>> run("gage runs -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | name=Room                    |
    | 2 | hello     | completed | name=Moon                    |
    <0>

Archive the runs.

    >>> run("gage archive --all -y")  # +parse
    Copied 2 runs
    Runs archived to '{name}'
    ⤶
    Use 'gage restore {x} --all' to restore these runs later.
    Use 'gage archive --list' to show available archives.
    <0>

    >>> assert x == name

Show available archives.

    >>> run("gage archive --list")  # +parse +table
    | name                | runs | last archived |
    |---------------------|------|---------------|
    | {x}                 | 2    | {}            |
    <0>

    >>> assert x == name

The archived runs deleted after being copied.

    >>> run("gage runs -s")  # +fails TODO delete is pending
    | # | operation | status | description                     |
    |---|-----------|--------|---------------------------------|
    <0>

The archive is located in a unique directory under the archives
directory. We can get the current archives directory using the `check`
command with the verbose option.

    >>> run("gage check -v", cols=120)  # +parse +table
    {}
    | archives_directory    | {archives_dir:path} |
    <0>

    >>> os.listdir(archives_dir)  # +parse
    ['{subdir:uuid4}']

The archive name is located in the archive subdirectory in a file named
`.archive`. This is an implementation detail but we can verify it.

    >>> cat(path_join(archives_dir, subdir, ".archive"))  # +parse
    1{x}

    >>> assert x == name

Use `--delete` to delete the archive.

    >>> run(f"gage archive --delete {name} -y")  # +parse
    Deleted archive '{x}'
    <0>

    >>> assert x == name

    >>> run("gage archive --list")
    ... # +skip=WINDOWS_FIX table expanded to fill default cols it seems
    ... # We can brute force this with +table but it'd be nicer to have
    ... # the formatting consistent across platforms so we don't have to
    ... # keep hacking the test.
    | name | runs | last archived |
    |------|------|---------------|
    <0>

Confirm that the archive directory is deleted.

    >>> os.listdir(archives_dir)
    []

## Errors

Archive without a run spec or `--all`.

    >>> run("gage archive")
    gage: Specify one or more runs to archive or use '--all'.
    ⤶
    Use 'gage list' to show available runs.
    ⤶
    Try 'gage archive -h' for additional help.
    <1>

Rename args:

    >>> run("gage archive --rename")
    Error: Option '--rename' requires 2 arguments.
    <2>

    >>> run("gage archive --rename foo")
    Error: Option '--rename' requires 2 arguments.
    <2>

Incompatible options:

    >>> run("gage archive --list --delete xxx")
    list and delete cannot be used together.
    ⤶
    Try 'gage archive -h' for help.
    <1>

    >>> run("gage archive --list --name xxx")
    list and name cannot be used together.
    ⤶
    Try 'gage archive -h' for help.
    <1>

    >>> run("gage archive --list --rename xxx yyy")
    list and rename cannot be used together.
    ⤶
    Try 'gage archive -h' for help.
    <1>

### Archive Location

How to specify/configure an archive location that's outside Gage home?
E.g. If I want to create a "Gage Archives" directory under `~/Dropbox`,
how can I tell Gage at large to use that location?

E.g. in user config: `archives.dir`

Also, via env var, e.g. `ARCHIVES_DIR` and `GAGE_ARCHIVES` (in keeping
with how runs dir can be specified).

As with runs dir, this scheme of centralizing archives will end up
lumping all of the archives under one roof, so `gage archive --list`
ends up listing archives across multiple projects. This sounds great
provided we can see which projects are associated with which archives.
OR, we concede that an archive can be the home of runs from different
projects.
