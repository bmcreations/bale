# Shared checkout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Store each large tracked file once under `BALE_HOME/clones/` and make every worktree's copy a filesystem clone of it, at `bale new` time and in the sweep for baled trees.

**Architecture:** A new `bale_core::clones` module lists a tree's large tracked files (git blobs, or Git LFS objects through their pointer), keeps a content-addressed store of them, and places clones with `clonefile` (macOS) or `FICLONE` (Linux). `bale new` checks out with `--no-checkout` and fills large files from the store before `git checkout-index`. The sweep shares baled trees; `bale gc` drops store files no live tree lists.

**Tech Stack:** Rust, `libc` (`clonefile`, `ioctl(FICLONE)`), git CLI through `bale_core::git::Git`, `sha2` (already a dependency) for LFS.

**Spec:** `docs/superpowers/specs/2026-10-30-bale-shared-checkout-design.md`; gate results in `docs/bench/2026-10-shared-checkout.md`.

## Global Constraints

- Only files of `clone_min_bytes` or more are shared; default `1 << 20` (1 MiB), the same as `stash_min_bytes`.
- Store layout: `BALE_HOME/clones/git/<oid[..2]>/<oid>` and `BALE_HOME/clones/lfs/<sha256[..2]>/<sha256>`. One store for all repos: an oid or SHA-256 names the same bytes in any repo. This replaces the spec's per-repo folder; Task 8 updates the spec.
- Store files are written once (clone to a temp name, then rename without replacing), mode `0o444`, never modified.
- A worktree file is replaced only when its bytes match the key: `git hash-object --no-filters` equals the index oid, or for LFS its SHA-256 equals the pointer's. Modified files are left alone.
- Replacement clones to `<file>.bale-share` next to the file, sets the original mode and nanosecond mtime, then renames over it.
- Skip any path whose `filter` attribute is set to anything but `lfs`, or whose `text` or `eol` attribute is set; skip the whole tree when `core.autocrlf` is `true` or `input`. Only modes `100644` and `100755` are candidates.
- The sweep shares only `Level::Baled` trees that are not pinned, takes `WorkspaceLock::try_acquire` without waiting, and skips busy trees.
- Store and tree must be on the same volume (`st_dev`); otherwise, and on filesystems without clones, every operation is a no-op that reports why, never an error.
- Windows: not in v1. `clone_file` returns `ErrorKind::Unsupported`.
- Tests use the `support::Fixture` temp repo and its temp `BALE_HOME`; never the real `~/.bale` or `~/.local/bin/bale`.
- Commits as `Brandon McAnsh <git@bmcreations.dev>`, no trailers.

## Review Focus

1. A large tracked file the user edited and didn't commit must survive a share untouched (Task 4, `share_leaves_modified_file`).
2. A tree on a filesystem without clones (Linux CI runs ext4) must share nothing and report it, not fail the sweep or `bale new` (Task 4, `share_reports_unsupported`; Task 5, `new_without_clones_checks_out_normally`).
3. An executable large file keeps mode `0o755` after being replaced or placed (Task 4, `share_keeps_mode_and_mtime`; Task 5 asserts mode).
4. A tree locked by a hook mid-restore is skipped by the sweep, not waited on (Task 6, `sweep_share_skips_locked_tree`).
5. A path with an `eol=crlf` or custom `filter` attribute is never placed from raw store bytes (Task 2, `candidates_skip_converted_paths`).

---

### Task 1: Clone primitive

**Files:**
- Create: `crates/bale-core/src/clones/mod.rs`, `crates/bale-core/src/clones/reflink.rs`
- Modify: `crates/bale-core/src/lib.rs` (add `pub mod clones;`), `crates/bale-core/src/ops/unstash.rs:717-738` (replace the private `clonefile` with `crate::clones::reflink::clone_file`)
- Test: inline `#[cfg(test)] mod tests` in `reflink.rs`

**Interfaces:**
- Produces: `pub fn clone_file(src: &Path, dst: &Path) -> io::Result<()>` (fails if `dst` exists; macOS `clonefile` with `CLONE_NOFOLLOW = 0x0001`; Linux create `dst` then `ioctl(dst_fd, libc::FICLONE, src_fd)`, removing `dst` on failure; elsewhere `ErrorKind::Unsupported`). `pub fn same_volume(a: &Path, b: &Path) -> io::Result<bool>` (compares `MetadataExt::dev`; `false` off Unix).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn clone_file_copies_bytes_to_new_inode() {
    let d = tempfile::tempdir().unwrap();
    let (a, b) = (d.path().join("a"), d.path().join("b"));
    std::fs::write(&a, b"hello").unwrap();
    match clone_file(&a, &b) {
        Err(e) if e.kind() == io::ErrorKind::Unsupported => return, // ext4, Windows
        r => r.unwrap(),
    }
    assert_eq!(std::fs::read(&b).unwrap(), b"hello");
    assert_ne!(ino(&a), ino(&b));
}

#[test]
fn clone_file_refuses_existing_dst() {
    // write a and b; clone_file(a, b) is Err and b still holds its old bytes
}

#[test]
fn same_volume_true_within_tempdir() { /* two files in one tempdir -> true */ }
```

On Linux, `FICLONE` failing with `EOPNOTSUPP`, `EXDEV` or `EINVAL` maps to `ErrorKind::Unsupported`.

- [ ] **Step 2: Run** `cargo test -p bale-core clones::reflink` — FAIL, module missing.
- [ ] **Step 3: Implement** `clone_file` and `same_volume`; switch `unstash.rs:540` to `clone_file`.
- [ ] **Step 4: Run** `cargo test -p bale-core` — PASS, including the existing unstash tests.
- [ ] **Step 5: Commit** `feat: add a clone primitive for macOS and Linux`

---

### Task 2: Large tracked files of a tree

**Files:**
- Create: `crates/bale-core/src/clones/tracked.rs`
- Modify: `crates/bale-core/src/git.rs` (add `run_stdin`), `crates/bale-core/src/clones/mod.rs`
- Test: `crates/bale-core/tests/clones.rs` (starts with `mod support;`)

**Interfaces:**
- Produces, in `clones/mod.rs`:
  - `#[derive(Debug, Clone, PartialEq, Eq, Hash)] pub enum Key { Git(String), Lfs(String) }` (oid hex; LFS SHA-256 hex)
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub struct Candidate { pub path: PathBuf /* relative to tree */, pub key: Key, pub size: u64, pub mode: u32 /* 0o644 or 0o755 */ }`
- Produces, in `clones/tracked.rs`:
  - `pub fn candidates(tree: &Path, min_bytes: u64) -> Result<Vec<Candidate>>` — from the index: `ls-files -s -z`, sizes from one `cat-file --batch-check` over the oids, attributes from one `check-attr -z --stdin filter text eol`, pointers from one `cat-file --batch` over LFS-filtered oids under 1024 bytes. Returns an empty list when `core.autocrlf` is `true` or `input`.
  - `pub fn parse_lfs_pointer(bytes: &[u8]) -> Option<(String, u64)>`
- Produces, in `git.rs`: `pub fn run_stdin(&self, args: &[&str], input: &[u8]) -> Result<String>`, same env, overrides and retries as `run`.

- [ ] **Step 1: Write the failing tests** in `tests/clones.rs`

```rust
#[test]
fn candidates_lists_large_files_only() {
    // commit big.bin (2 MiB) and small.txt (10 bytes) in fx.repo
    let c = candidates(&fx.repo, 1 << 20).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].path, Path::new("big.bin"));
    assert_eq!(c[0].size, 2 << 20);
    assert!(matches!(&c[0].key, Key::Git(oid) if *oid == git(&fx.repo, &["rev-parse", "HEAD:big.bin"]).trim()));
}

#[test]
fn candidates_keep_exec_mode_and_skip_symlinks() { /* 100755 file -> mode 0o755; a symlink to big.bin is absent */ }

#[test]
fn candidates_skip_converted_paths() {
    // .gitattributes: "crlf.bin eol=crlf" and "f.bin filter=custom"; both 2 MiB -> neither listed
}

#[test]
fn parse_lfs_pointer_reads_oid_and_size() {
    let p = b"version https://git-lfs.github.com/spec/v1\noid sha256:3eda6971f9d9c8064d45aa485b84c88516bb06bed1fb6eff9b4f16870034478f\nsize 1073741824\n";
    assert_eq!(parse_lfs_pointer(p), Some(("3eda6971f9d9c8064d45aa485b84c88516bb06bed1fb6eff9b4f16870034478f".into(), 1073741824)));
    assert_eq!(parse_lfs_pointer(b"not a pointer"), None);
}

#[test]
fn candidates_key_lfs_files_on_pointer() {
    // skip (return) when `git lfs version` fails; else track *.bin with LFS, commit 2 MiB big.bin
    // -> one candidate, Key::Lfs(sha256 of the file), size 2 << 20
}
```

- [ ] **Step 2: Run** `cargo test -p bale-core --test clones` — FAIL.
- [ ] **Step 3: Implement** `run_stdin`, `candidates`, `parse_lfs_pointer`.
- [ ] **Step 4: Run** `cargo test -p bale-core --test clones` — PASS.
- [ ] **Step 5: Commit** `feat: list a tree's large tracked files`

---

### Task 3: The clone store

**Files:**
- Create: `crates/bale-core/src/clones/store.rs`
- Modify: `crates/bale-core/src/lib.rs` (`pub fn clones_dir(&self) -> PathBuf` returning `self.0.join("clones")`), `crates/bale-core/src/clones/mod.rs`
- Test: `crates/bale-core/tests/clones.rs`

**Interfaces:**
- Consumes: `clone_file`, `same_volume` (Task 1); `Key` (Task 2).
- Produces:
  - `pub struct CloneStore { root: PathBuf }`
  - `pub fn open(home: &BaleHome) -> io::Result<CloneStore>` (creates `clones/`)
  - `pub fn path(&self, key: &Key) -> PathBuf` (layout in Global Constraints)
  - `pub fn has(&self, key: &Key) -> bool`
  - `pub fn adopt(&self, key: &Key, from: &Path) -> io::Result<bool>` — clones a file the caller has verified into the store; `Ok(false)` if already stored; temp name `<path>.tmp-<pid>-<n>`, mode `0o444`, rename that never replaces (`renamex_np(RENAME_EXCL)` on macOS, `renameat2(RENAME_NOREPLACE)` on Linux); `EEXIST` counts as stored.
  - `pub fn place(&self, key: &Key, dst: &Path, mode: u32, mtime: Option<SystemTime>) -> io::Result<()>` — clones to `<dst>.bale-share`, sets `mode` and, when given, `mtime` (atime = mtime), renames over `dst`; removes the temp file on any error.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn adopt_then_place_round_trips() {
    // store.adopt(&key, &file) == Ok(true); again == Ok(false); store file mode & 0o777 == 0o444
    // place into a missing dst with mode 0o755 and mtime T: bytes equal, mode 0o755, mtime == T to the ns
}

#[test]
fn place_over_existing_file_leaves_no_temp() { /* dst replaced; no "*.bale-share" left in its dir */ }
```

Guard both with the `Unsupported` early return from Task 1.

- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** `cargo test -p bale-core --test clones` — PASS.
- [ ] **Step 5: Commit** `feat: add the clone store`

---

### Task 4: Share an existing tree, and `bale share`

**Files:**
- Create: `crates/bale-core/src/clones/share.rs`
- Modify: `crates/bale-core/src/config.rs` (fields below, plus the `Config` literal in `sweep.rs:826` tests), `crates/bale-core/src/registry.rs` (`Record`), `crates/bale-core/src/trace.rs` (`impl Traced for ShareReport`), `crates/bale-cli/src/main.rs` (`Share { target }`)
- Test: `crates/bale-core/tests/clones.rs`, `crates/bale-cli/tests/cli.rs`

**Interfaces:**
- Consumes: Tasks 1-3; `WorkspaceLock::try_acquire`; `busy::busy_holders`; `Registry`.
- Produces:
  - Config: `pub share_checkouts: bool` (RawConfig `share_checkouts: Option<bool>`, repo over machine, default `true`) and `pub clone_min_bytes: u64` (`DEFAULT_CLONE_MIN_BYTES: u64 = 1 << 20`).
  - Record: `#[serde(default, skip_serializing_if = "Option::is_none")] pub shared_head: Option<String>` — `head` at the last share that ran to the end.
  - `#[derive(Debug, Default)] pub struct ShareReport { pub files: u64, pub bytes: u64, pub stored: u64, pub skipped: Vec<(PathBuf, String)>, pub reason: Option<String> }` (`reason` set when the whole tree was skipped: "locked", "busy", "other volume", "clones unsupported", "disabled")
  - `pub fn share(home: &BaleHome, id: &str) -> Result<ShareReport>`, traced as op `"share"` through `trace::op`.
- Behaviour: lock without waiting; skip busy; skip if `!share_checkouts`; skip if store and tree aren't on one volume; `candidates`; verify all `Key::Git` paths with one `hash-object --no-filters --stdin-paths`, LFS paths with `store::sha256_file`; for each match, `adopt` if missing, then `place` with the file's own mode and mtime; stop at the first `Unsupported` with `reason = "clones unsupported"`; then `update-index -q --refresh` and set `shared_head`.
- `bale share <target>` prints `shared N files (X MiB), stored M` or `skipped: <reason>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn share_two_worktrees_uses_one_store_file() {
    // 2 MiB big.bin committed; w1, w2 = fx.add_worktree; register both; config.toml clone_min_bytes = 1
    // share(w1) then share(w2): (macOS) w2 report.files == 1, store has 1 file, both bytes intact,
    // `git status --porcelain` empty in both, record.shared_head == Some(record.head)
}

#[test]
fn share_leaves_modified_file() { /* overwrite big.bin in w1 with other bytes -> skipped contains big.bin, bytes unchanged */ }

#[test]
fn share_keeps_mode_and_mtime() { /* 0o755 big.bin with mtime T -> after share mode 0o755, mtime == T ns */ }

#[test]
fn share_reports_unsupported() {
    // cfg(not(target_os = "macos")): on a filesystem without clones, report.reason == Some("clones unsupported"),
    // files == 0, file bytes and inode unchanged, Ok returned
}

#[test]
fn share_skips_locked_tree() { /* hold WorkspaceLock -> reason == Some("locked") */ }
```

CLI test in `cli.rs`: `bale share <path>` exits 0 and prints `shared` or `skipped:`.

- [ ] **Step 2: Run** `cargo test -p bale-core --test clones share_` — FAIL.
- [ ] **Step 3: Implement** config fields, record field, `share`, `Traced`, CLI.
- [ ] **Step 4: Run** `cargo test -p bale-core && cargo test -p bale-cli` — PASS.
- [ ] **Step 5: Commit** `feat: share a tree's large tracked files through the clone store`

---

### Task 5: New checkouts start shared

**Files:**
- Modify: `crates/bale-core/src/ops/new.rs:51-71`
- Create: `fn materialize(home: &BaleHome, tree: &Path, cfg: &Config) -> Result<()>` in `crates/bale-core/src/clones/share.rs` (`pub(crate)`)
- Test: `crates/bale-core/tests/new_gc.rs`

**Interfaces:**
- Consumes: `candidates`, `CloneStore`, `Config::{share_checkouts, clone_min_bytes}`.
- Produces: `new_untraced` runs `worktree add --no-checkout` (with or without `-b`, as today), then `materialize`, then `register_untraced`.
- `materialize`: `read-tree HEAD`; if sharing is on and the volume matches, `place` each candidate already in the store (mode from the candidate, mtime `None`), stopping placement at the first `Unsupported`; then `checkout-index -a -q` (fills everything else, runs LFS smudge); then `update-index -q --refresh`; then `adopt` each candidate not yet stored, after verifying it as Task 4 does. A failure in `read-tree` or `checkout-index` fails `bale new` and goes through the existing undo of created dirs.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn new_places_stored_files() {
    // commit 2 MiB big.bin (mode 100755); clone_min_bytes = 1; new_workspace "a" then "b"
    // b/big.bin bytes == committed bytes, mode 0o755, `git status --porcelain` empty in b,
    // (macOS) store holds 1 file and b/big.bin's inode differs from the store file's
}

#[test]
fn new_without_clones_checks_out_normally() { /* share_checkouts = false -> same checks, store dir empty or absent */ }

#[test]
fn new_creates_and_registers() // existing test still passes unchanged
```

- [ ] **Step 2: Run** `cargo test -p bale-core --test new_gc` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p bale-core` — PASS.
- [ ] **Step 5: Commit** `feat: fill new worktrees from the clone store`

---

### Task 6: Sweep shares baled trees

**Files:**
- Modify: `crates/bale-core/src/ops/sweep.rs` (after the per-record loop at L169-184, before the macOS block at L185)
- Test: `crates/bale-core/tests/sweep.rs`

**Interfaces:**
- Consumes: `share` (Task 4).
- Produces: `fn share_rows(home: &BaleHome, records: &[Record], dry_run: bool) -> Vec<(String, Action, String)>` — for each record with `level == Level::Baled`, `!pinned`, `shared_head.as_deref() != Some(&head)`: on `dry_run` report "dry run, not applied", else call `share`. Rows use `Action::Report("share".into())` and the report's summary text, appended to the sweep's rows.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn sweep_shares_baled_tree_once() { /* baled tree with 2 MiB big.bin -> first sweep has a "share" row, shared_head set; second sweep has none */ }

#[test]
fn sweep_does_not_share_hot_tree() { /* level Hot -> no "share" row, file inode unchanged */ }

#[test]
fn sweep_share_skips_locked_tree() { /* lock held -> row text contains "locked", shared_head stays None */ }
```

- [ ] **Step 2: Run** `cargo test -p bale-core --test sweep share` — FAIL. **Step 3: Implement.** **Step 4: Run** `cargo test -p bale-core` — PASS.
- [ ] **Step 5: Commit** `feat: share baled trees in the sweep`

---

### Task 7: gc reclaims store files

**Files:**
- Create: `crates/bale-core/src/clones/gc.rs`
- Modify: `crates/bale-core/src/ops/gc.rs` (`collect_store`, `GcReport` gains `pub clones_bytes_freed: u64`; CLI prints it next to `store_bytes_freed`)
- Test: `crates/bale-core/tests/clones.rs`

**Interfaces:**
- Produces: `pub fn collect(store: &CloneStore, live: &HashSet<Key>, grace: Duration, dry_run: bool) -> Result<u64>` — removes store files whose key isn't in `live` and whose mtime is older than `grace` (`store::gc::GC_GRACE`, 1 h); also removes `*.tmp-*` files older than `grace`.
- `collect_store` builds `live` from `candidates(record.path, cfg.clone_min_bytes)` for every record whose path exists. If any record is unreadable or any listing fails, it removes nothing and pushes `("clones", msg)` to `errors`, as the stash store does.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn gc_keeps_listed_and_young_files() { /* listed key kept; unlisted key with fresh mtime kept */ }

#[test]
fn gc_removes_unlisted_old_files() { /* unlisted key with mtime 2 h ago removed; returned bytes == its size */ }

#[test]
fn gc_removes_nothing_when_a_listing_fails() { /* a record whose path is not a git tree -> 0 removed, error pushed */ }
```

Use `filetime`-free mtime setting through `std::fs::File::set_modified`.

- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** `cargo test --workspace` — PASS.
- [ ] **Step 5: Commit** `feat: reclaim unreferenced clone store files in gc`

---

### Task 8: Docs and a check on this Mac

**Files:**
- Modify: `docs/architecture.md` (new "Shared checkouts" section after "Stash and the store"; add `clones/` to "State on disk"; add `share_checkouts` and `clone_min_bytes` to the defaults line; drop "reflinked … trees" from "Not built"), `docs/superpowers/specs/2026-10-30-bale-shared-checkout-design.md` (store layout is global, not per repo), `AGENTS.md` if it lists modules.

- [ ] **Step 1:** Write the docs. Run `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` — PASS.
- [ ] **Step 2:** Release build in a worktree; with a temp `BALE_HOME` on the same volume, run `bale share` against two temp `git worktree add` copies of the Android repo and record the `df` change. Expected about 1.7 GiB for two trees, as in the gate. Do not touch `~/.local/bin/bale` or `~/.bale`.
- [ ] **Step 3: Commit** `docs: describe shared checkouts`
