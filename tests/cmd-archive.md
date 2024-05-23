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
      -d, --delete archive      Delete the specified archive.
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

## To Do

Nothing working now:

    >>> run("gage archive")  # -space
    TODO archive:
    Args(runs=[], name='', delete='', edit='',
         list=False, where='', all=False, yes=False)
    <0>

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
