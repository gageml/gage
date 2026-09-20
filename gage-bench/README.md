# Gage Bench

Benchmarks for the Gage store.

## Background

The Gage store is a bare Git repository. Every object is a commit under
`refs/gage/object/<id>`. Reads go through one long-lived `git cat-file
--batch-command` child per store, so a lookup costs a pipe round trip;
writes emit loose objects (blob, tree, commit) directly into `objects/`
and use `git update-ref` for the ref. Selection is served by a SQLite
index rebuilt from the refs. None of these choices are conventional for
a data store, so their behavior at scale has to be measured rather than
assumed. This crate exists to keep those measurements repeatable. See
`results/store/` for the results that document the shift from a
process-per-op shape to the current one.

The bench answers four questions:

- How the store behaves under writes at a chosen scale. Write throughput is not
  a design goal, but its characteristics have to be known.
- Whether everything written reads back as written, including tombstones, links,
  and dataset membership.
- How the common read operations perform: open, resolve, get, query, full
  iteration, dataset listing, and content streaming.
- Whether a change made things faster or slower. Every run is saved with its
  inputs and the code version that produced it, and any run can be compared
  against a saved one.

The crate originally held a serialization format comparison that informed the
session summary cache. That question was settled and the code was removed.

## The `store` bench

One run builds a fresh store under a temporary directory and proceeds through
four phases.

1. **Populate.** Notes, sessions, large sessions, and datasets are generated
   from a seeded generator and written through the public store API. A fraction
   of the notes are then edited, a fraction of the sessions are grown by
   appending lines, and a fraction of the notes are deleted. Every operation is
   timed individually.
2. **Sizes.** Repository size, object count, ref count, and index file size are
   recorded before and after `gc`.
3. **Verify.** Every created id resolves to an object of its type with the
   expected tombstone state. Counts by type through the index match the expected
   counts. A sample of notes reads back with the value that was written. Every
   ref's commit parents are fully attributed to its `parent` and link files.
   Every dataset lists the expected number of members. A failure aborts the run
   and leaves the run directory for inspection.
4. **Reads.** Each read operation is repeated `--iterations` times: open with a
   warm index, resolve by full id and by prefix, get by id, query by name with a
   limit, full iteration over notes, query ordered by modified time, dataset
   session listing, and streaming one session's content for a regular and a
   large session. Random access is `--random-reads` reads by id drawn uniformly
   from every live object, once through the type-agnostic resolve and read path
   and once through `NoteStore::get` over live notes. Rebuilding the index from
   nothing is measured once, since it walks every commit.

Sessions are synthetic JSONL. Session content in the store is opaque bytes, so
the generator produces the shape at a chosen size rather than reading real
transcripts, which keeps a run deterministic and independent of the machine.

## Inputs

| Flag               | Default | Meaning                                                     |
| ------------------ | ------- | ----------------------------------------------------------- |
| `--notes`          | 2000    | Notes to create                                             |
| `--note-bytes`     | 256     | Bytes per note value                                        |
| `--sessions`       | 200     | Sessions to add                                             |
| `--session-kb`     | 64      | KiB of content per session                                  |
| `--large-sessions` | 2       | Large sessions to add, reported as their own rows           |
| `--large-kb`       | 5120    | KiB of content per large session                            |
| `--datasets`       | 10      | Datasets to create                                          |
| `--dataset-size`   | 20      | Sessions linked into each dataset                           |
| `--edit-pct`       | 10      | Percent of notes edited and sessions grown after creation   |
| `--delete-pct`     | 5       | Percent of notes deleted after creation                     |
| `--iterations`     | 10      | Repetitions of each read operation                          |
| `--random-reads`   | 500     | Reads by id drawn uniformly from the whole population       |
| `--seed`           | 1       | Generator seed; the same seed produces the same objects     |
| `--baseline`       | none    | `latest` or a saved results file to compare against         |
| `--keep`           | off     | Keep the run directory and its store after a successful run |

Session sizes are fixed rather than drawn from a range. Blob cost is linear in
bytes, so a range adds variance without information. The large sessions exist to
exercise the tail: streaming, size accounting, and pack behavior for content in
the megabytes.

## Outputs

The run prints three tables: timings per operation (count, total, p50, p95, max,
operations per second), sizes in bytes, and counts. With `--baseline` it also
prints per-operation p50 deltas and per-measure byte deltas as percentages.

Every run is saved to `gage-bench/results/store/<stamp>.json` in the
source tree, so a baseline can be committed with the change it measured. The file holds the bench name, the run stamp,
the code version from `git describe --always --dirty`, every input value, and
every result. The temporary run directory is deleted on success unless `--keep`
is given.

## Running

Build in release mode. Debug timings do not represent the store.

The three tiers are argument files under `gage-bench/configs/`. An `@FILE`
argument expands to the file's lines, one option per line, with `#` comments
and blank lines dropped. Anything typed after the `@FILE` overrides the file's
value, so a profile is a starting point rather than a fixed run.

**Smoke test**

A few seconds.

```shell
cargo run -p gage-bench --release -- store @gage-bench/configs/smoke.conf
```

**Normal**

A few minutes. The defaults, and the scale baselines are kept at.

```shell
cargo run -p gage-bench --release -- store @gage-bench/configs/normal.conf
```

**Heavy**

Tens of minutes, for scale questions and for measuring the tail.

```shell
cargo run -p gage-bench --release -- store @gage-bench/configs/heavy.conf
```

Overriding one option of a profile:

```shell
cargo run -p gage-bench --release -- store @gage-bench/configs/normal.conf --iterations 20
```

Comparing against the previous run at the same scale:

```shell
cargo run -p gage-bench --release -- store --baseline latest
```

A comparison across different inputs is flagged with a warning; the deltas are
not meaningful in that case.

## Reading the numbers

A write is one `git update-ref` launch plus loose object files; a read is a
few round-trips on the store's long-lived `cat-file` process; a query is one
SQLite statement plus one read per row. Those are the floors the p50 columns
show. Every run is one process on a machine with nothing else controlled.
Run-to-run variation on the same code has not been measured beyond two
smoke-scale runs that agreed within a few percent on most rows, so a delta
from one run of each version is a lead, not a finding, until the same code has
been run more than once.

The index rebuild figure is the cost of a full reconcile: reading every commit
and its link files once. The warm open figure is the cost of the ref diff when
nothing changed.
