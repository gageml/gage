# Archive Util

   >>> from gage._internal.archive_util import *

Archives are stored under a root archives directory. Each archive
directory contains one or more archives runs. Archives are named using a
sortable UUID.

    >>> make_archive_id()  # +parse
    '{:uuid4}'

Archive directories are written under the directory returned by
`var.archives_dir()`. This directory can be specified by the
`GAGE_ARCHIVES` environment variable or by calling
`var.set_archives_dir()`.

Use a temp directory to store the archives.

    >>> from gage._internal import var

    >>> tmp = make_temp_dir()
    >>> var.set_archives_dir(tmp)

Archives are associated with *names*. A name is a user-generated string
that is written to `.archive` in the archive directory. Use
`make_archive_dir` to create a new archive directory and associate it
with a name.

    >>> dirname = make_archive_dir("hello")

Confirm that the new archive directory is located under tmp.

    >>> ls(tmp)  # +parse
    {x:uuid4}/.archive

    >>> compare_paths(dirname, path_join(tmp, x))
    True

Confirm that the archive is associated with the expected name.

    >>> cat(path_join(dirname, ".archive"))
    1hello

Note that the name is encoded with a leading schema value.

Find an archive directory by name using `find_archive_directory`.

    >>> find_archive_dir("hello")  # +parse
    '{x}'

    >>> assert x == dirname

An archive name can be read using `get_archive_name`.

    >>> get_archive_name(dirname)
    'hello'

An archive can be renamed using `set_archive_name`.

    >>> set_archive_name(dirname, "hello-2")

    >>> get_archive_name(dirname)
    'hello-2'

    >>> cat(path_join(dirname, ".archive"))
    1hello-2
