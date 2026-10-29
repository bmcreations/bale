# Warm tier checks

## Task 0: does Xcode accept a rewritten build output? (2026-10-29)

Restoring a stashed file with the same bytes, mode and nanosecond mtime is not enough. If the file gets a new inode, Xcode relinks every binary built from it. Writing the bytes back into the same inode costs nothing.

Setup: Flipcash iOS (`Flipcash` scheme, simulator, `CODE_SIGNING_ALLOWED=NO`), a `git clone --shared` checkout with in-tree `-derivedDataPath .dd`. Xcode 27.1 (27A9275), macOS 27.2, M3 Max, 36 GB. Load average 4–18 during the timed builds. One run per case, warm cache. The full build took 354 s; a build with no edit takes 15–17 s.

Each case rewrote files with no source edit, restored mode and mtime with `os.utime(ns=...)`, then ran one incremental build.

| Case | Files rewritten | Build | Link steps | Files the build rewrote |
|---|---|---|---|---|
| No change | 0 | 15.3–16.7 s | 0 | – |
| Final product `Flipcash.debug.dylib` (451 MB), new inode | 1 | 15.6 s | 0 | 0 |
| Same file, same inode | 1 | 15.6 s | 0 | 0 |
| Control: same file, mtime moved 5 s | 1 | 19.5 s | 0 | 1 (copied again) |
| T1: every file ≥ 1 MiB under `.dd`, new inode | 711 (7.73 GB) | 45.8 s | 16 | 39 |
| T2: same files, same inode | 711 | 17.6 s | 0 | 0 |
| T3: same files, new inode via `cp` (xattrs kept) | 711 | 43.0 s | 16 | 39 |

The 16 link steps are the per-architecture intermediate binaries of Flipcash, NotificationContent, NotificationService and ShareExtension. The final product passes with a new inode because Xcode copies it from those intermediates rather than linking it. T1 to T3 leave out `XCBuildData/` and `SDKStatCaches.noindex/`, which change on every build.

T3 rules out lost extended attributes as the cause. The only attribute on build output is `com.apple.provenance` (132,481 entries; all 712 files ≥ 1 MiB carry it), plus `com.apple.xcode.CreatedByBuildSystem` on 9 directories. macOS adds `com.apple.provenance` to new files by itself, so T1 kept it too.

So a stashed file must keep its inode: stash truncates it to 0 bytes in place, and unstash writes the bytes back into it. Writing 7.7 GB back in place took 7.3 s from page cache, single-threaded Python. Some files under `SourcePackages/checkouts` are read-only, so the owner write bit has to be set before writing and the mode restored after.

Two more results:

- Rust's `std::fs::copy` clones on APFS: three copies of a 1 GiB file changed free space by 0–1 MiB. It can't be used to restore duplicates, though, because a clone is a new inode.
- Gradle wasn't tested; it fingerprints content, not inodes.
