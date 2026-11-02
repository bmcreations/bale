# bale: compress idle build output in place

Second item from `docs/bench/2026-10-alternatives-summary.md` ("Worth building"). This spec covers macOS APFS;
Linux and Windows are follow-ups.

## Problem

Only Claude Code has a hook that fires before a build, so only its trees get stashed. Trees from T3 Code,
Codex, Cursor and Conductor stay at full size until they are baled after 4 h idle, and baling deletes the
output, so the next build in that tree starts cold.

The benchmarks found a state in between. APFS transparent compression of files of 1 MiB or more freed 4.1 of
5.7 GiB under an iOS `Build/` and 893 MiB of 1,989 MiB under an Android `build/`. It needs no restore: the
kernel decompresses on read. Once the nanosecond mtimes were put back, a no-op Xcode build linked nothing and
Gradle re-ran nothing. The cost was a no-op build of 22 s against 14 s uncompressed, and 4-5 s on a leaf edit.

## How bale compresses a file

The benchmarks used `afsctool`, which bale can't depend on in-process: the Rust port, `applesauce`, is
GPL-3.0, and bale is MIT OR Apache-2.0. The MIT `decmpfs` crate (0.1.5) writes a new file and renames it over
the original. That gives every file a new inode, which makes Xcode relink, and it doesn't keep mtimes.

bale writes the decmpfs format itself, on the file's own descriptor:

1. `fstat` the file and keep `(size, mtime_ns, ino)`. Skip it unless it is a regular file with one link, at
   least `stash_min_bytes`, and not already `UF_COMPRESSED`.
2. Read it and compress each 64 KiB block with LZFSE through the system's libcompression
   (`compression_encode_buffer`). Build the resource fork: `(blocks + 1)` little-endian u32 offsets, then the
   blocks. Skip the file if the fork isn't smaller than the file.
3. `fstat` again. If `(size, mtime_ns, ino)` changed, skip the file.
4. `fsetxattr` `com.apple.ResourceFork` with the fork and `com.apple.decmpfs` with the 16-byte header
   (`cmpf`, type 12, uncompressed size), then `fsync`. The file isn't flagged yet, so reads still return the
   data fork and these attributes are inert.
5. `fchflags(UF_COMPRESSED)`. From this call on, reads come from the resource fork, and APFS frees the data
   fork.
6. `futimens` back to the mtime from step 1, then `fsync`.

There is no truncate. A truncate after step 5 sets the logical size to 0 and loses the content. A truncate
before step 5, which is the order `afsctool` appears to use, leaves an empty unflagged file if bale crashes
in between.

A spike checked this order on 2026-11-01 (macOS 27.2, APFS):

| Check | 3 MiB text file | 512 MiB file on a disk image |
|---|---|---|
| Inode | unchanged | unchanged |
| SHA-256 after | matches | matches, after detaching and re-attaching the image |
| Nanosecond mtime after step 6 | `.123456789` kept | `.123456789` kept |
| Space | `st_blocks` 6,144 to 208, same as `afsctool` | `df` used 524,312 to 349,244 KiB; `st_blocks` agrees |
| `ls -lO` | `compressed` | `compressed` |
| Append to the compressed file | succeeds; the file is decompressed and unflagged | not run |

Step 4 changes the file's mtime, so a crash between steps 4 and 6 leaves correct content with a new mtime.
That costs a relink of that file, not a wrong build. To repair it, bale writes `(path, ino, size, mtime_ns)`
for each file to `BALE_HOME/compress/<id>.jsonl`, fsynced before step 4. The sweep replays the journal before
it does anything else in that tree: a file that is now compressed, or unflagged and the same size, gets its
mtime back. Then the journal is deleted.

## Where it fits

Compression, like stash, is a flag on the record, not a level: `record.compressed` holds the time of the last
pass and how many bytes it freed.

- **Which trees.** Hot or baled trees without a restore hook, idle for `idle_compress` (default 30 min), not
  stashed, not pinned. A restore-hook tree is stashed instead, because stash frees more (5.8 against 4.1 GiB
  on the same tree).
- **Which directories.** A new `BuildSystem::compress()` method, defaulting to `stash()`. The benchmarks only
  measured DerivedData `Build/` and Gradle `build/`, so nothing wider goes in until it has been measured.
- **Guards.** The tree's lock without waiting, and the busy check over every compressed directory before and
  after the pass. The busy check has to include DerivedData outside the tree (`global_owned`). Before
  building this, check that `busy_holders_many` gets those paths, since stash passes the same list.
- **Again.** A tree whose `last_activity` is newer than `record.compressed` gets another pass. Files that are
  already compressed are skipped by flag, so a second pass only reads what the build wrote since.
- **Sweep order.** A new rule in `sweep::decide`, between stash (3) and keep (4).
- **Trace.** A `compress` op line with files, bytes before and after, and duration.
- **Docs.** Levels table, sweep order and config defaults in `docs/architecture.md`; the README's level list
  and `config.toml` keys.

`up`, `cold` and stash need no change: deleting and reading a compressed file work as usual. Unstash never
sees one, because a stashed tree isn't compressed.

## Tests

- Unit tests on the format: the offset table and header bytes, checked against a file `afsctool` compressed
  when it is installed, and skipped when it isn't.
- Round trip on a temp file: inode, SHA-256 and nanosecond mtime unchanged, `UF_COMPRESSED` set, `st_blocks`
  lower.
- Guards: a hard-linked file, a file that changes between steps 1 and 3, an incompressible file, and a busy
  tree are each left alone.
- Journal replay: a journal left by an interrupted pass restores the mtime and is removed.
- Sweep: a tree without a restore hook is compressed after `idle_compress`, and a restore-hook tree is not.

All of these are macOS only. CI runs them on `macos-latest`.

## Not covered

- Linux: `btrfs filesystem defragment -czstd` kept inode, mtime and ctime in the Linux benchmark. In code that
  is `BTRFS_IOC_DEFRAG_RANGE` with a compression type. It is its own change.
- Windows NTFS compression.
- Cargo `target/`, which the benchmarks didn't measure.
- The read cost during a large incremental build, and files held open by a running build. The busy check keeps
  bale away from the second.

## Open question

When is a compressed tree baled? If it is baled at `idle_baled` (4 h), as now, compression only saves space
between 30 min and 4 h of idle time. A compressed build directory is about 30% of its size on iOS and about
55% on Android, so keeping it longer, such as until `idle_cold`, keeps more warm builds for less disk. The
benchmarks don't say how often a tree is built again after 4 h; the usage trace could.
