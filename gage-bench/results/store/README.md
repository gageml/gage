# Store bench results

## 01 - 04

Normal-scale runs (2,000 notes, 200 sessions, 2 large sessions, 10 datasets),
one per change, p50 in milliseconds. The factor after each value is the speedup
over the column to its left: `+2.0` is twice as fast, `-1.5` is 1.5 times
slower.

| Operation (p50 ms)            |     01 |     02 |            |    03 |            |    04 |           |
| ----------------------------- | -----: | -----: | ---------: | ----: | ---------: | ----: | --------: |
| note create                   |   17.6 |   10.5 |       +1.7 |  5.59 |  +1.9 [^2] |  1.26 | +4.4 [^3] |
| note edit                     |   30.7 |   16.6 |       +1.9 |  12.9 |  +1.3 [^5] |  7.07 |      +1.8 |
| note delete                   |   28.0 |   14.3 |       +2.0 |  10.8 |  +1.3 [^5] |  6.93 |      +1.6 |
| session add                   |   19.8 |   12.6 |       +1.6 |  7.67 |       +1.6 |  2.45 |      +3.1 |
| session add (large)           |    101 |   78.4 |  +1.3 [^4] |  77.1 |       +1.0 |   109 | -1.4 [^4] |
| dataset sessions add          |    183 |   60.3 |       +3.0 |  58.4 |       +1.0 |  12.3 | +4.8 [^3] |
| gc                            |  1,354 |  1,283 |       +1.1 | 1,317 |       -1.0 | 1,294 |      +1.0 |
| open (warm index)             |   4.04 |   4.16 |       -1.0 |  3.94 |       +1.1 |  6.59 |      -1.7 |
| open (rebuild index)          | 29,475 | 11,635 |  +2.5 [^1] |   251 | +46.4 [^2] |   246 |      +1.0 |
| random read by id             |   6.45 |   0.63 | +10.3 [^1] |  0.63 |       -1.0 |  0.61 |      +1.0 |
| note get                      |   7.57 |   0.65 |      +11.7 |  0.73 |       -1.1 |  0.77 |      -1.1 |
| session content read          |   7.48 |   2.29 |       +3.3 |  2.65 |       -1.2 |  2.52 |      +1.1 |
| session content read (large)  |   27.2 |   21.7 |       +1.3 |  22.2 |       -1.0 |  21.4 |      +1.0 |
| query name, limit 20          |    122 |   2.56 | +47.6 [^1] |  2.54 |       +1.0 |  2.48 |      +1.0 |
| query modified desc, limit 20 |    123 |   2.24 |      +54.8 |  2.44 |       -1.1 |  2.19 |      +1.1 |
| iter all notes                | 11,724 |    219 | +53.5 [^1] |   235 |       -1.1 |   217 |      +1.1 |
| dataset sessions list         |    114 |   1.75 |      +65.0 |  1.78 |       -1.0 |  1.68 |      +1.1 |

Code versions: 01 `640f549`, 02 `fb28d28`, 03 `7ec331e`, 04 `511d77d`.

[^1]: **Persistent cat-file process.** Every read had been several `git`
    launches at about 0.6 ms each; `read_object` alone was about ten. One
    `git cat-file --batch-command` child per `Store` turns each into a pipe
    round-trip. Per-object reads drop by an order of magnitude and anything that
    reads many objects (queries, full iteration, dataset listing) by nearly two.
    The rebuild gains less because its remaining cost was the index, not git.

[^2]: **Index writes in transactions.** The index committed one SQLite
    transaction per object, and each WAL commit synced to disk. The reconcile
    now runs as one transaction and every write-through as one, with
    `synchronous=NORMAL`, which is safe for an index that is rebuilt from the
    refs whenever it is missing. The rebuild drops from 11.6 s to 251 ms and
    every write loses its sync.

[^3]: **Loose objects written directly.** Blobs, trees, and commits are written
    by the store as loose objects (SHA-1 over the header and bytes, zlib, rename
    into `objects/`) instead of through `hash-object`, `mktree`, and
    `commit-tree`. A create goes from eight `git` launches to one, `update-ref`,
    which stays with git for ref locking. Written objects are checked against
    `git hash-object`, `git mktree`, and `git commit-tree` in the test suite,
    and every test store ends with `git fsck --strict`.

[^4]: **Two samples.** `session add (large)` has a count of two per run and
    swings 40% between runs of the same code; its factors are not a measurement.
    The heavy tier's five large sessions are the minimum for a figure with
    meaning.

[^5]: **Prefix resolution.** An edit or delete resolves its target first with a
    `for-each-ref` glob, which is the one read still on a process launch and
    which grows with the count of loose refs before `gc`. Creates do not pay it,
    which is why edits and deletes gained less in 03 and 04.
