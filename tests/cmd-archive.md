---
test-options: +skip=WINDOWS_FIX # nagging table formatting inconsistencies
---

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
      -A, --archive name        Use an existing archive. Fails
                                if archive doesn't exit.
      -d, --delete name         Delete the specified archive.
                                Use '--list' to show archives.
      --delete-empty            Delete empty archives.
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
    Moved 2 runs
    Runs archived to '{name}'
    ⤶
    Use 'gage restore --archive {x}' to restore from this archive.
    <0>

    >>> assert x == name

Show available archives.

    >>> run("gage archive --list")  # +parse +table
    | name                | runs | last archived |
    |---------------------|------|---------------|
    | {x}                 | 2    | {}            |
    <0>

    >>> assert x == name

The archived runs are deleted after being copied.

    >>> run("gage runs -s")
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

## Restore Runs

Runs are restored from an archive using `restore` with the `--archive` option.

`restore` requires a valid archive name with `--archive`.

    >>> run("gage restore --archive invalid")
    gage: archive 'invalid' does not exist
    ⤶
    Use 'gage archive --list' to show available archives.
    <1>

Restore the runs from the archive. Gage assumes that all runs are
restored. Gage assumes the `--all` option, which is otherwise required
when restoring deleted runs when run specs aren't provided.

    >>> run(f"gage restore --archive {name} -y")
    Restored 2 runs
    <0>

    >>> run("gage ls -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | name=Room                    |
    | 2 | hello     | completed | name=Moon                    |
    <0>

    >>> run(f"gage ls -A {name} -s")
    | # | operation | status | description                     |
    |---|-----------|--------|---------------------------------|
    <0>

## Delete Empty Archives

Delete empty archives using `--delete-empty`.

    >>> run("gage archive -l")  # +parse +table
    | name              | runs | last archived |
    |-------------------|------|---------------|
    | {x}               | 0    | {}            |
    <0>

    >>> assert x == name

    >>> run("gage archive --delete-empty -y")
    Deleted 1 empty archive
    <0>

## Delete Archive

Use `--delete` to delete the archive. This permanently deletes the
archive runs.

Archive the current runs.

    >>> run("gage archive --all --name my-archive -y")
    Moved 2 runs
    Runs archived to 'my-archive'
    ⤶
    Use 'gage restore --archive my-archive' to restore from this archive.
    <0>

    >>> run("gage ls -s")
    | # | operation | status | description                     |
    |---|-----------|--------|---------------------------------|
    <0>

    >>> run("gage ls -A my-archive -s")
    | # | operation | status    | description                  |
    |---|-----------|-----------|------------------------------|
    | 1 | hello     | completed | name=Room                    |
    | 2 | hello     | completed | name=Moon                    |
    <0>

Delete requires a valid archive name.

    >>> run("gage archive --delete invalid")
    gage: archive 'invalid' does not exist.
    Use 'gage archive --list' to show available names.
    <1>

The user is notified that the deletion is permanent.

    >>> run(f"gage archive --delete my-archive", timeout=2)  # +parse +table
    | #  | name   | operation   | started         | status     |
    |----|--------|-------------|-----------------|------------|
    | 1  | {}     | hello       | {}              | completed  |
    | 2  | {}     | hello       | {}              | completed  |
    ⤶
    You are about to PERMANENTLY delete 2 runs. This cannot be undone.
    ⤶
    Continue? (y/N)
    <{:sigint}>

Delete the archive.

    >>> run(f"gage archive --delete my-archive -y")
    Deleted archive 'my-archive'
    <0>

    >>> run("gage archive --list")
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
