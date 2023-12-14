# SPDX-License-Identifier: Apache-2.0

from typing import *

import errno
import hashlib
import logging
import os
import re
import shutil
import stat
import subprocess
import tempfile
import time

__all__ = [
    "Chdir",
    "TempDir",
    "TempFile",
    "compare_paths",
    "copy_tree",
    "dir_size",
    "ensure_dir",
    "ensure_safe_delete_tree",
    "expand_path",
    "file_md5",
    "file_sha1",
    "file_sha256",
    "files_differ",
    "files_digest",
    "ls",
    "find_up",
    "is_readonly",
    "is_text_file",
    "make_dir",
    "make_executable",
    "make_temp_dir",
    "realpath",
    "delete_temp_dir",
    "safe_filename",
    "safe_list_dir",
    "safe_delete_tree",
    "set_readonly",
    "shorten_path",
    "standardize_path",
    "subpath",
    "test_windows_symlinks",
    "write_file",
]

log = logging.getLogger(__name__)


def standardize_path(path: str):
    return path.replace(os.path.sep, "/")


_text_ext = {
    ".csv",
    ".html",
    ".html",
    ".json",
    ".jsx",
    ".md",
    ".py",
    ".r",
    ".sh",
    ".ts",
    ".tsx.txt",
    ".yaml",
    ".yml",
    "js",
}

_binary_ext = {
    ".ai",
    ".bmp",
    ".gif",
    ".ico",
    ".jpeg",
    ".jpg",
    ".png",
    ".ps",
    ".psd",
    ".svg",
    ".tif",
    ".tiff",
    ".aif",
    ".mid",
    ".midi",
    ".mpa",
    ".mp3",
    ".ogg",
    ".wav",
    ".wma",
    ".avi",
    ".mov",
    ".mp4",
    ".mpeg",
    ".swf",
    ".wmv",
    ".7z",
    ".deb",
    ".gz",
    ".pkg",
    ".rar",
    ".rpm",
    ".tar",
    ".xz",
    ".z",
    ".zip",
    ".doc",
    ".docx",
    ".key",
    ".pdf",
    ".ppt",
    ".pptx",
    ".xlr",
    ".xls",
    ".xlsx",
    ".bin",
    ".pickle",
    ".pkl",
    ".pyc",
}

_control_chars = b"\n\r\t\f\b"

_printable_ascii = _control_chars + bytes(range(32, 127))

_printable_high_ascii = bytes(range(127, 256))


def is_text_file(path: str, ignore_ext: bool = False):
    import chardet

    # Adapted from https://github.com/audreyr/binaryornot under the
    # BSD 3-clause License
    if not os.path.exists(path):
        raise OSError(f"{path} does not exist")
    if not os.path.isfile(path):
        return False
    if not ignore_ext:
        ext = os.path.splitext(path)[1].lower()
        if ext in _text_ext:
            return True
        if ext in _binary_ext:
            return False
    try:
        with open(path, "rb") as f:
            sample = f.read(1024)
    except IOError:
        return False
    if not sample:
        return True
    low_chars = sample.translate(None, _printable_ascii)
    nontext_ratio1 = float(len(low_chars)) / float(len(sample))
    high_chars = sample.translate(None, _printable_high_ascii)
    nontext_ratio2 = float(len(high_chars)) / float(len(sample))
    likely_binary = (nontext_ratio1 > 0.3 and nontext_ratio2 < 0.05) or (
        nontext_ratio1 > 0.8 and nontext_ratio2 > 0.8
    )
    detected_encoding = chardet.detect(sample)
    decodable_as_unicode = False
    if (
        detected_encoding["confidence"] > 0.9
        and detected_encoding["encoding"] != "ascii"
    ):
        try:
            sample.decode(encoding=detected_encoding["encoding"] or "utf-8")
        except LookupError:
            pass
        except UnicodeDecodeError:
            pass
        else:
            decodable_as_unicode = True
    if likely_binary:
        return decodable_as_unicode
    if decodable_as_unicode:
        return True
    if b"\x00" in sample or b"\xff" in sample:
        return False
    return True


def ensure_dir(d: str):
    try:
        make_dir(d)
    except OSError as e:
        if e.errno != errno.EEXIST:
            raise


def make_dir(d: str):
    os.makedirs(realpath(d))


def ls(
    root: str,
    followlinks: bool = False,
    include_dirs: bool = False,
    unsorted: bool = False,
):
    paths: list[str] = []

    def relpath(path: str, name: str):
        return os.path.relpath(os.path.join(path, name), root)

    for path, dirs, files in os.walk(root, followlinks=followlinks):
        for name in dirs:
            if include_dirs or os.path.islink(os.path.join(path, name)):
                paths.append(relpath(path, name))
        for name in files:
            paths.append(relpath(path, name))
    return paths if unsorted else sorted(paths)


def find_up(
    relpath: str,
    start_dir: Optional[str] = None,
    stop_dir: Optional[str] = None,
    check: Callable[[str], bool] = os.path.exists,
):
    start_dir = os.path.abspath(start_dir) if start_dir else os.getcwd()
    stop_dir = realpath(stop_dir) if stop_dir else _user_home()

    cur = start_dir
    while True:
        maybe_target = os.path.join(cur, relpath)
        if check(maybe_target):
            return maybe_target
        if realpath(cur) == stop_dir:
            return None
        parent = os.path.dirname(cur)
        if parent == cur:
            return None
        cur = parent

    # `parent == cur` above should be the definitive terminal case
    assert False


def _user_home():
    return os.path.expanduser("~")


def realpath(path: str):
    # Workaround for https://bugs.python.org/issue9949
    try:
        link = os.readlink(path)
    except OSError:
        return os.path.realpath(path)
    else:
        path_dir = os.path.dirname(path)
        return os.path.abspath(os.path.join(path_dir, _strip_windows_prefix(link)))


def _strip_windows_prefix(path: str):
    if os.name != "nt":
        return path
    if path.startswith("\\\\?\\"):
        return path[4:]
    return path


def expand_path(path: str):
    return os.path.expanduser(os.path.expandvars(path))


def files_differ(path1: str, path2: str):
    if os.stat(path1).st_size != os.stat(path2).st_size:
        return True
    f1 = open(path1, "rb")
    f2 = open(path2, "rb")
    with f1, f2:
        while True:
            buf1 = f1.read(1024)
            buf2 = f2.read(1024)
            if buf1 != buf2:
                return True
            if not buf1 or not buf2:
                break
    return False


def files_digest(paths: list[str], root_dir: str):
    md5 = hashlib.md5()
    for path in paths:
        normpath = _path_for_digest(path)
        md5.update(_encode_file_path_for_digest(normpath))
        md5.update(b"\x00")
        _apply_digest_file_bytes(os.path.join(root_dir, path), md5)
        md5.update(b"\x00")
    return md5.hexdigest()


def _path_for_digest(path: str):
    return path.replace(os.path.sep, "/")


def _encode_file_path_for_digest(path: str):
    return path.encode("UTF-8")


def _apply_digest_file_bytes(path: str, d: Any):
    buf_size = 1024 * 1024
    with open(path, "rb") as f:
        while True:
            buf = f.read(buf_size)
            if not buf:
                break
            d.update(buf)


def compare_paths(p1: str, p2: str):
    return _resolve_path(p1) == _resolve_path(p2)


def _resolve_path(p: str):
    return realpath(os.path.expanduser(p))


def file_sha256(path: str, use_cache: bool = True):
    if use_cache:
        cached_sha = try_cached_sha(path)
        if cached_sha:
            return cached_sha

    return _gen_file_hash(path, hashlib.sha256())


def file_sha1(path: str):
    return _gen_file_hash(path, hashlib.sha1())


def _gen_file_hash(path: str, hash: "hashlib._Hash"):
    with open(path, "rb") as f:
        while True:
            data = f.read(102400)
            if not data:
                break
            hash.update(data)
    return hash.hexdigest()


def try_cached_sha(for_file: str):
    try:
        f = open(_cached_sha_filename(for_file), "r")
    except IOError:
        return None
    else:
        return f.read().rstrip()


def _cached_sha_filename(for_file: str):
    parent, name = os.path.split(for_file)
    return os.path.join(parent, f".gage-cache-{name}.sha")


def write_cached_sha(sha: str, for_file: str):
    with open(_cached_sha_filename(for_file), "w") as f:
        f.write(sha)


def file_md5(path: str):
    import hashlib

    return _gen_file_hash(path, hashlib.md5())


class _TempBase:
    def __init__(self, prefix: str = "gage-", suffix: str = "", keep: bool = False):
        self._prefix = prefix
        self._suffix = suffix
        self._keep = keep
        self.path = self._init_temp(self._prefix, self._suffix)

    def __enter__(self):
        return self

    @staticmethod
    def _init_temp(prefix: str, suffix: str) -> str:
        raise NotImplementedError()

    def __exit__(self, *exc: Any):
        if not self._keep:
            self.delete()

    def keep(self):
        self._keep = True

    def delete(self) -> None:
        raise NotImplementedError()


class TempDir(_TempBase):
    @staticmethod
    def _init_temp(prefix: str, suffix: str):
        return tempfile.mkdtemp(prefix=prefix, suffix=suffix)

    def delete(self):
        delete_temp_dir(self.path)


class TempFile(_TempBase):
    @staticmethod
    def _init_temp(prefix: str, suffix: str):
        f, path = tempfile.mkstemp(prefix=prefix, suffix=suffix)
        os.close(f)
        return path

    def delete(self):
        os.remove(self.path)


def make_temp_dir(prefix: str = ""):
    return tempfile.mkdtemp(prefix=prefix)


def delete_temp_dir(path: str):
    assert os.path.dirname(path) == tempfile.gettempdir(), path
    try:
        shutil.rmtree(path)
    except Exception as e:
        if log.getEffectiveLevel() <= logging.DEBUG:
            log.exception("rmtree %s", path)
        else:
            log.error("error removing %s: %s", path, e)


def safe_delete_tree(path: str, force: bool = False):
    """Removes path if it's not top level or user dir."""
    assert not _top_level_dir(path), path
    assert path != os.path.expanduser("~"), path
    if os.path.isdir(path):
        shutil.rmtree(path)
    elif os.path.isfile(path):
        os.remove(path)
    elif not force:
        raise ValueError(f"{path} does not exist")


def ensure_safe_delete_tree(path: str):
    try:
        safe_delete_tree(path, force=True)
    except OSError as e:
        if e.errno != errno.ENOENT:
            raise


def _top_level_dir(path: str):
    abs_path = os.path.abspath(path)
    parts = [p for p in re.split(r"[/\\]", abs_path) if p]
    if os.name == "nt":
        return len(parts) <= 2
    return len(parts) <= 1


def test_windows_symlinks():
    if os.name != "nt":
        return
    with TempDir() as tmp:
        os.symlink(tempfile.gettempdir(), os.path.join(tmp.path, "link"))


def strip_trailing_sep(path: str):
    if path and path[-1] in ("/", "\\"):
        return path[:-1]
    return path


def strip_leading_sep(path: str):
    if path and path[0] in ("/", "\\"):
        return path[1:]
    return path


def ensure_trailing_sep(path: str, sep: Optional[str] = None):
    sep = sep or os.path.sep
    if path[-1:] != sep:
        path += sep
    return path


def subpath(path: str, start: str, sep: Optional[str] = None):
    if path == start:
        raise ValueError(path, start)
    start_with_sep = ensure_trailing_sep(start, sep)
    if path.startswith(start_with_sep):
        return path[len(start_with_sep) :]
    raise ValueError(path, start)


def symlink(target: str, link: str):
    if os.name == "nt":
        _windows_symlink(target, link)
    else:
        os.symlink(target, link)


def copy_file(src: str, dest: str):
    shutil.copyfile(src, dest)
    shutil.copymode(src, dest)


def _windows_symlink(target: str, link: str):
    if os.path.isdir(target):
        args = ["mklink", "/D", link, target]
    else:
        args = ["mklink", link, target]
    try:
        subprocess.check_output(args, shell=True, stderr=subprocess.STDOUT)
    except subprocess.CalledProcessError as e:
        err_msg = e.output.decode(errors="ignore").strip()
        _maybe_symlink_error(err_msg, e.returncode)
        raise OSError(e.returncode, err_msg) from e


def _maybe_symlink_error(err_msg: str, err_code: int):
    if "You do not have sufficient privilege to perform this operation" in err_msg:
        raise SystemExit(
            "You do not have sufficient privilege to perform this operation\n\n"
            "For help, see "
            "https://my.guild.ai/docs/windows#symbolic-links-privileges-in-windows",
            err_code,
        )


def touch(filename: str):
    open(filename, "ab").close()
    now = time.time()
    os.utime(filename, (now, now))


def ensure_file(filename: str):
    if not os.path.exists(filename):
        touch(filename)


def copy_tree(src: str, dest: str, preserve_links: bool = True):
    try:
        # dirs_exist_ok was added in Python 3.8:
        # https://docs.python.org/3/library/shutil.html#shutil.copytree
        shutil.copytree(src, dest, symlinks=preserve_links, dirs_exist_ok=True)
    except TypeError as e:
        assert "got an unexpected keyword argument 'dirs_exist_ok'" in str(e), e
        # Drop this fallback when drop support for Python 3.7
        # pylint: disable=deprecated-module
        from distutils import dir_util

        dir_util.copy_tree(src, dest, preserve_symlinks=preserve_links)


class Chdir:
    _save = None

    def __init__(self, path: str):
        self.path = path

    def __enter__(self):
        self._save = os.getcwd()
        os.chdir(self.path)

    def __exit__(self, *exc: Any):
        assert self._save is not None
        os.chdir(self._save)


def dir_size(dir: str):
    size = 0
    for root, dirs, names in os.walk(dir):
        for name in dirs + names:
            size += os.path.getsize(os.path.join(root, name))
    return size


def safe_list_dir(path: str) -> list[str]:
    try:
        return os.listdir(path)
    except OSError:
        return []


def shorten_path(
    path: str, max_len: int = 28, ellipsis: str = "\u2026", sep: str = os.path.sep
):
    if len(path) <= max_len:
        return path
    parts = _shorten_path_split_path(path, sep)
    if len(parts) == 1:
        return parts[0]
    assert all(parts), parts
    r = [parts.pop()]  # Always include rightmost part
    if parts[0][0] == sep:
        l = []
        pop_r = False
    else:
        # Relative path, always include leftmost part
        l = [parts.pop(0)]
        pop_r = True
    while parts:
        len_l = sum((len(s) + 1 for s in l))
        len_r = sum((len(s) + 1 for s in r))
        part = parts.pop() if pop_r else parts.pop(0)
        side = r if pop_r else l
        if len_l + len_r + len(part) + len(ellipsis) >= max_len:
            break
        side.append(part)
        pop_r = not pop_r
    shortened = os.path.sep.join(
        [os.path.sep.join(l), ellipsis, os.path.sep.join(reversed(r))]
    )
    if len(shortened) >= len(path):
        return path
    return shortened


def _shorten_path_split_path(path: str, sep: str) -> list[str]:
    """Splits path into parts.

    Leading and repeated '/' chars are prepended to the
    part. E.g. "/foo/bar" is returned as ["/foo", "bar"] and
    "foo//bar" as ["foo", "/bar"].
    """
    if not path:
        return []
    parts = path.split(sep)
    packed = []
    blanks = []
    for part in parts:
        if part == "":
            blanks.append("")
        else:
            packed.append(sep.join(blanks + [part]))
            blanks = []
    if len(blanks) > 1:
        packed.append(sep.join(blanks))
    return packed


def make_executable(path: str):
    os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)


def write_file(
    filename: str,
    contents: str,
    append: bool = False,
    readonly: bool = False,
):
    opts = "a" if append else "w"
    with open(filename, opts) as f:
        f.write(contents)
    if readonly:
        set_readonly(filename)


def set_readonly(filename: str, readonly: bool = True):
    mode = (
        stat.S_IREAD | stat.S_IRGRP | stat.S_IROTH
        if readonly
        else stat.S_IWUSR | stat.S_IREAD
    )
    os.chmod(filename, mode)


def is_readonly(filename: str):
    return not os.access(filename, os.W_OK)


def safe_filename(s: str):
    if os.name == "nt":
        s = re.sub(r"[:<>?]", "_", s).rstrip()
    return re.sub(r"[/\\]+", "_", s)
