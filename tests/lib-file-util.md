# File Utils

The module `gage._internal.file_util` implements advanced file utilities.

    >>> from gage._internal.file_util import *

## Test for file difference

Use `files_differ()` to check if two files differ.

Write some files to test.

    >>> tmp = make_temp_dir()

    >>> with open(path_join(tmp, "a"), "wb") as f:
    ...     _ = f.write(b"abc123")

    >>> with open(path_join(tmp, "b"), "wb") as f:
    ...     _ = f.write(b"abc1234")

    >>> with open(path_join(tmp, "c"), "wb") as f:
    ...     _ = f.write(b"abc321")

    >>> with open(path_join(tmp, "d"), "wb") as f:
    ...     _ = f.write(b"abc123")

Compare the files:

    >>> files_differ(path_join(tmp, "a"), path_join(tmp, "a"))
    False

    >>> files_differ(path_join(tmp, "a"), path_join(tmp, "b"))
    True

    >>> files_differ(path_join(tmp, "a"), path_join(tmp, "c"))
    True

    >>> files_differ(path_join(tmp, "a"), path_join(tmp, "d"))
    False

Compare links:

    >>> symlink("a", path_join(tmp, "link-to-a"))

    >>> symlink("link-to-a", path_join(tmp, "link-to-link-to-a"))

    >>> files_differ(
    ...     path_join(tmp, "a"),
    ...     path_join(tmp, "link-to-a")
    ... )
    False

    >>> files_differ(
    ...     path_join(tmp, "a"),
    ...     path_join(tmp, "link-to-link-to-a")
    ... )
    False

    >>> files_differ(
    ...     path_join(tmp, "link-to-a"),
    ...     path_join(tmp, "link-to-link-to-a")
    ... )
    False

## Files digest

A single digest is generated for a directory using `files_digest()`.

Use `textorbinary` sample files to generate a digest.

    >>> sample_dir = sample("textorbinary")

    >>> files_digest(ls(sample_dir), sample_dir)
    '6779694aab1ddab3d4e551a8a3168baa'

## Testing text files

Use `is_text_file()` to test if a file is text or binary. This is used
to provide a file viewer for text files.

The test uses known file extensions as an optimization. To test the
file content itself, we need to ignore extensions:

    >>> def is_text(sample_path):
    ...     path = sample("textorbinary", sample_path)
    ...     return is_text_file(path, ignore_ext=True)

Test various files:

    >>> is_text("cookiecutter.json")
    True

    >>> is_text("empty.pyc")
    False

    >>> is_text("empty.txt")
    True

    >>> is_text("hello.py")
    True

    >>> is_text("hello_world.pyc")
    False

    >>> is_text("lena.jpg")
    False

    >>> is_text("lookup-error")
    False

    >>> is_text("lookup-error.txt")
    True

A non-existing file generates an error:

    >>> is_text("non-existing")  # +wildcard
    Traceback (most recent call last):
    OSError: .../samples/textorbinary/non-existing does not exist

Directories aren't text files:

    >>> is_text(".")
    False

## File digests

The functions `file_sha1()`, `file_sha256()`, and `file_md5()` generate
file digests using the corresponding algorithm.

    >>> file_sha1(sample("textorbinary", "lena.jpg"))
    'af17f9c0a80505922933edb713bf30ec25916fbf'

    >>> file_sha256(sample("textorbinary", "lena.jpg"), use_cache=False)
    '54e97bc5e2a8744022f3c57c08d2a36566067866d9cabfbdde131e99a485134a'

    >>> file_md5(sample("textorbinary", "lena.jpg"))
    '596ab5651006a9cf0f1ed722278f02fe'

## Safe rmtree check

The function `safe_rmtree` will fail if the specified path is a
top-level directory. Top level is defined as either the root or a
directory in the root.

The function `_top_level_dir` is used for this test.

    >>> from gage._internal.file_util import _top_level_dir

Tests:

    >>> _top_level_dir(os.path.sep)
    True

    >>> _top_level_dir(path_join(os.path.sep, "foo"))
    True

    >>> _top_level_dir(path_join(os.path.sep, "foo", "bar"))
    False

    >>> _top_level_dir(".")
    False

## Shorten dirs

The function `shorten_path()` is used to shorten directories by
removing path segments and replacing them with an ellipsis ('...') as
needed to keep them under a specified length.

    >>> shorten = lambda s, max_len: shorten_path(s, max_len, sep="/")

Any paths under the specified length are returned unmodified:

    >>> shorten("/foo/bar/baz", max_len=20)
    '/foo/bar/baz'

If a path is longer than `max_len`, the function tries to shorten it
by replacing path segments with an ellipsis.

If a path has fewer then two segments, it is returned unmodified
regardless of the max length:

    >>> shorten("foo", max_len=0)
    'foo'

If a shortened path is not actually shorter than the original path,
the original path is returned unmodified.

    >>> shorten("/a/b", max_len=0)
    '/a/b'

    >>> shorten("/aaa/bbb/ccc", max_len=12)
    '/aaa/bbb/ccc'

The function attempts to include as much of the original path in the
shortened version as possible. It will always at least include the
last segment in a shortened version.

    >>> shorten("/aaa/bbb/ccc", max_len=0)
    '/…/ccc'

If able to, the function includes path segments from both the left and
right sides.

    >>> shorten("/aaa/bbbb/ccc", max_len=12)
    '/aaa/…/ccc'

The function checks each segment side, starting with the right side
and then alternating, to include segment parts. It stops when the
shortened path would exceed max length.

    >>> shorten("/aaa/bbbb/cccc/ddd", max_len=17)
    '/aaa/…/cccc/ddd'

    >>> shorten("/aaa/bbbb/cccc/ddd", max_len=16)
    '/aaa/…/cccc/ddd'

    >>> shorten("/aaa/bbbb/cccc/ddd", max_len=12)
    '/aaa/…/ddd'

    >>> shorten("/aaa/bbbb/cccc/ddd", max_len=0)
    '/…/ddd'

The same rules applied to relative paths:

    >>> shorten("aaa/bbbb/cccc/ddd", max_len=16)
    'aaa/…/cccc/ddd'

    >>> shorten("aaa/bbbb/cccc/ddd", max_len=15)
    'aaa/…/cccc/ddd'

    >>> shorten("aaa/bbbb/cccc/ddd", max_len=11)
    'aaa/…/ddd'

    >>> shorten("aaa/bbbb/cccc/ddd", max_len=0)
    'aaa/…/ddd'

### Splitting paths for shorten dir

The shorten dir algorithm uses `util._shorten_path_split_path`, which
handles cases of leading and repeating path separators by appending
them to the next respective part.

    >>> from gage._internal.file_util import _shorten_path_split_path
    >>> ds_split = lambda s: _shorten_path_split_path(s, "/")

Examples:

    >>> ds_split("")
    []

    >>> ds_split("/")
    ['/']

    >>> ds_split("foo")
    ['foo']

    >>> ds_split("/foo")
    ['/foo']

    >>> ds_split("foo/bar")
    ['foo', 'bar']

    >>> ds_split("/foo/bar")
    ['/foo', 'bar']

    >>> ds_split("/foo/bar/")
    ['/foo', 'bar']

    >>> ds_split("//foo//bar")
    ['//foo', '/bar']

## Testing subdirectories

    >>> subpath("/foo/bar", "/foo", "/")
    'bar'

    >>> subpath("/foo/bar", "/bar", "/")
    Traceback (most recent call last):
    ValueError: ('/foo/bar', '/bar')

    >>> subpath("/foo", "/foo", "/")
    Traceback (most recent call last):
    ValueError: ('/foo', '/foo')

    >>> subpath("/foo/", "/foo", "/")
    ''

    >>> subpath("", "", "/")
    Traceback (most recent call last):
    ValueError: ('', '')

    >>> subpath("/", "/", "/")
    Traceback (most recent call last):
    ValueError: ('/', '/')

## File names

When writing files the `safe_filename` function is used to ensure the
file name is valid for a platform.

On Windows, the function replaces colons with underscores.

    >>> safe_filename("hello:there")  # +skip TODO how to limit to Windows?
    'hello_there'

On all platforms, the function replaces any possible path separator
with underscore.

    >>> safe_filename("hello/there\\friend")
    'hello_there_friend'
