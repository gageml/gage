# Project utils

Gage uses the concept of a "project" to locate a directory container for
a current directory. This container is consulted for various topics:

- Location of Gage config (`gage.yaml`, `gage.json`, etc.)
- Default operation namespace

Projects are located using `find_project_dir`, which is applied on a
directory.

    >>> from gage._internal.project_util import find_project_dir

`find_project_dir` checks the specified directory and its parents for a
project marker, returning the first directory containing a marker. A
second argument can be specified to stop the search at the specified
path. The function returns None if it can't find a project directory.

Create a directory to search.

    >>> root = make_temp_dir()

`root` is empty and so can't be a project.

    >>> find_project_dir(root, root)  # +pprint
    None

If we create a project marker in root, it becomes a project.

    >>> touch(path_join(root, ".vscode"))

    >>> find_project_dir(root, root)  # +parse
    '{x:path}'

    >>> assert x == root

Create a subdirectory of root.

    >>> subdir = path_join(root, "subdir")
    >>> make_dir(subdir)

The subdir itself is not a project dir.

    >>> find_project_dir(subdir, subdir)  # +pprint
    None

When we include root in the search, it's selected as the project dir.

    >>> find_project_dir(subdir, root)  # +parse
    '{x:path}'

    >>> assert x == root

Create a subdirectory within subdir.

    >>> subdir2 = path_join(subdir, "subdir")
    >>> make_dir(subdir2)

Look for the project in the new sub dir.

    >>> find_project_dir(subdir2, subdir2)  # +pprint
    None

    >>> find_project_dir(subdir2, subdir)  # +pprint
    None

    >>> find_project_dir(subdir2, root)  # +parse
    '{x:path}'

    >>> assert x == root

Create a project marker in subdir.

    >>> touch(path_join(subdir, "gage.yaml"))

Find the project starting from subdir 2.

    >>> find_project_dir(subdir2, subdir2)  # +pprint
    None

    >>> find_project_dir(subdir2, subdir)  # +parse
    '{x:path}'

    >>> assert x == subdir

    >>> find_project_dir(subdir2, root)  # +parse
    '{x:path}'

    >>> assert x == subdir
