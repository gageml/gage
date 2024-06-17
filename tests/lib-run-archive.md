# Archive Util

Run archive support is provided by `run_archive`.

    >>> from gage._internal.run_archive import *

Archives are user-defined containers that runs are moved in and out of.
For details on moving runs, see [`lib-var.md`](lib.var.md).

When a run is archived, it is no longer active and does not appear in
runs list.

## Define an Archive

Archives consist of a unique archive ID and a user-defined name. They
are defined under the runs directory in a `.archive` subdirectory.

Use `make_archive` to create an archive definition under a runs
directory.

Create a runs directory.

    >>> runs_dir = make_temp_dir()

Create an archive under the runs dir.

    >>> archive = make_archive("Test", runs_dir)

    >>> archive.get_name()
    'Test'

The archive has a unique ID (uuid).

    >>> archive.get_id()  # +parse
    '{archive_id:uuid4}'

    >>> assert archive_id == archive.get_id()

Gage writes a JSON file under `.archive` using the archive ID and an
time sortable ID.

    >>> ls(runs_dir)  # +parse
    .archives/{x:uuid4}/{:uuid4}.json

    >>> assert x == archive_id

Use `iter_archives` to iterate the list of archive definitions.

    >>> for archive in iter_archives(runs_dir):
    ...     print(archive)  # +parse
    <ArchiveRef id="{x:uuid4}" name="Test">

    >>> assert x == archive_id

## Archive for Name

Use `archive_for_name` to load an archive by its name.

    >>> archive_for_name("Test", runs_dir)  # +parse
    <ArchiveRef id="{x}" name="Test">

    >>> assert x == archive_id

Gage raises `ArchiveNotFoundError` if it can't find an archive with the
specified name.

    >>> archive_for_name("invalid", runs_dir)
    Traceback (most recent call last):
    gage._internal.run_archive.ArchiveNotFoundError: invalid

## Update an Archive

Use `update_archive` to modify archive properties.

Rename the archive:

    >>> archive = archive_for_name("Test", runs_dir)

    >>> update_archive(archive, name="Test 2", runs_dir=runs_dir)  # +parse
    <ArchiveRef id="{x}" name="Test 2">

    >>> assert x == archive_id

    >>> for archive in iter_archives(runs_dir):  # +parse
    ...     print(archive)  # +parse
    <ArchiveRef id="{}" name="Test 2">

    >>> archive_for_name("Test", runs_dir)
    Traceback (most recent call last):
    gage._internal.run_archive.ArchiveNotFoundError: Test

    >>> archive_for_name("Test 2", runs_dir)  # +parse
    <ArchiveRef id="{}" name="Test 2">

Gage modifies archives by adding a new JSON file, which contains the
latest properties.

    >>> ls(runs_dir, natsort=False)  # +parse
    .archives/{x:uuid4}/{:uuid4}.json
    .archives/{y:uuid4}/{:uuid4}.json

    >>> assert x == y == archive_id

## Delete an Archive

Use `delete_archive` to mark an archive as deleted.

    >>> delete_archive(archive_id, runs_dir)

Gage does not delete the archive files but instead adds a `.deleted`
marker.

    >>> ls(runs_dir, natsort=False)  # +parse
    .archives/{x:uuid4}/{:uuid4}.json
    .archives/{y:uuid4}/{:uuid4}.json
    .archives/{z:uuid4}/{:uuid4}.deleted

    >>> assert x == y == z == archive_id
