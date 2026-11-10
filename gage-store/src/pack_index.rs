//! Lookup of an object in the store's packs, without a `git` process.
//!
//! `git gc` moves loose objects into a pack and deletes the loose
//! files, so a writer that looks only for a loose file takes a packed
//! object for a missing one and writes it again. The lookup here reads
//! the pack index files under `objects/pack/`.
//!
//! A version 2 pack index is an 8-byte header, a fanout table of 256
//! big-endian `u32` counts, and the SHAs of the pack's objects in
//! ascending order. Fanout entry `b` is the number of objects whose
//! first SHA byte is at most `b`, so entries `b - 1` and `b` bound the
//! run of SHAs that start with `b`, and a binary search of that run
//! decides membership.
//!
//! The lookup answers "not found" for anything it cannot read: an
//! index of another version, an index a concurrent `gc` deleted, a
//! short file. The caller then writes a loose object, which is always
//! correct; a duplicate of a packed object is removed by the next
//! `gc`.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The magic number and version that open a version 2 pack index.
const HEADER: [u8; 8] = [0xff, b't', b'O', b'c', 0, 0, 0, 2];
const FANOUT_ENTRY_LEN: u64 = 4;
const FANOUT_LEN: u64 = 256 * FANOUT_ENTRY_LEN;
const SHA_LEN: u64 = 20;

/// True when a pack under `store_path` holds the object `sha` and the
/// pack was marked as in use. The caller may then leave the object
/// unwritten.
///
/// Marking sets the pack file's modification time to now, as git does
/// when it reuses a packed object. An object reused for a commit not
/// yet written can be unreachable at this moment, and `gc` deletes
/// unreachable objects it takes for old; the fresh time keeps the
/// pack's objects from being taken for old. A cruft pack holds the
/// unreachable objects `gc` has set aside for deletion and is not
/// used, again as in git.
pub(crate) fn reuse_packed(store_path: &Path, sha: &[u8; 20]) -> bool {
    let Some(pack) = pack_holding(store_path, sha) else {
        return false;
    };
    match touch(&pack) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(pack = %pack.display(), "pack not marked as in use, object rewritten: {e}");
            false
        }
    }
}

/// Set the modification time of `path` to now.
pub(crate) fn touch(path: &Path) -> io::Result<()> {
    File::open(path)?.set_modified(SystemTime::now())
}

/// The pack file under `store_path` that holds `sha`, cruft packs
/// left out.
fn pack_holding(store_path: &Path, sha: &[u8; 20]) -> Option<PathBuf> {
    let pack_dir = store_path.join("objects").join("pack");
    let entries = match fs::read_dir(&pack_dir) {
        Ok(entries) => entries,
        // A store that has never been packed has no pack directory.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(dir = %pack_dir.display(), "packs not listed: {e}");
            return None;
        }
    };
    for entry in entries {
        let index = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                tracing::warn!(dir = %pack_dir.display(), "pack entry not read: {e}");
                continue;
            }
        };
        if index.extension().is_none_or(|ext| ext != "idx")
            || index.with_extension("mtimes").exists()
        {
            continue;
        }
        match index_holds(&index, sha) {
            Ok(true) => return Some(index.with_extension("pack")),
            Ok(false) => {}
            // A concurrent gc deletes an index it has replaced.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(index = %index.display(), "pack index not read: {e}"),
        }
    }
    None
}

/// Whether the pack index at `index` lists `sha`. An index that is
/// not version 2 lists nothing this reader can find.
fn index_holds(index: &Path, sha: &[u8; 20]) -> io::Result<bool> {
    let mut file = File::open(index)?;
    let mut header = [0u8; 8];
    file.read_exact(&mut header)?;
    if header != HEADER {
        return Ok(false);
    }
    let first = u64::from(sha[0]);
    let mut low = match first {
        0 => 0,
        _ => fanout(&mut file, first - 1)?,
    };
    let mut high = fanout(&mut file, first)?;
    let shas = HEADER.len() as u64 + FANOUT_LEN;
    let mut found = [0u8; 20];
    while low < high {
        let mid = low + (high - low) / 2;
        file.seek(SeekFrom::Start(shas + mid * SHA_LEN))?;
        file.read_exact(&mut found)?;
        match found.cmp(sha) {
            std::cmp::Ordering::Equal => return Ok(true),
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid,
        }
    }
    Ok(false)
}

/// Fanout entry `byte`: the number of objects whose first SHA byte is
/// at most `byte`.
fn fanout(file: &mut File, byte: u64) -> io::Result<u64> {
    let mut entry = [0u8; 4];
    file.seek(SeekFrom::Start(
        HEADER.len() as u64 + byte * FANOUT_ENTRY_LEN,
    ))?;
    file.read_exact(&mut entry)?;
    Ok(u64::from(u32::from_be_bytes(entry)))
}
