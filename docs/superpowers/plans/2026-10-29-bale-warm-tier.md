# Warm Tier (Stash) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a tree has been idle for 10 minutes, move the large files out of its build output into bale's own content-addressed store, and put them back before the next prompt within the restore budget.

**Architecture:** A store under `$BALE_HOME/store` holds zstd blobs keyed by the SHA-256 of their content. A changed file is stored as a `--patch-from`-style delta against the blob the same path held at the tree's previous stash. `bale stash` writes a per-tree manifest, then deletes the stashed files. `bale unstash` rebuilds them in parallel, checks each hash, and restores mode and mtime so incremental builds stay incremental. The sweep stashes idle trees. Claude Code's `UserPromptSubmit` and `PreToolUse` hooks unstash before the agent acts. Cold trees served over NFS are the next plan, not this one.

**Tech Stack:** Rust 2021, existing `bale-core` / `bale-hooks` / `bale-cli` crates, `zstd-safe` (new), `sha2`, `walkdir`, `serde_json`.

**Spec:** `docs/code-vm-design.md` (PR #5, branch `docs/code-vm-design`). Read its "Tiers", "Per-turn delta" and "Decisions" sections first.

## Global Constraints

- Restore budget: p50 at most 2 s; p95 3–4 s, with 5 s acceptable (spec, Decisions).
- Stash after T = 10 minutes idle by default (spec, Lifecycle).
- Only files of at least 1 MiB (1,048,576 bytes) are stashed by default; smaller files stay in the tree (spec, Tiers).
- Build output is only valid at the path it was built at: a manifest restores to its own tree's paths, never another tree's (spec, Per-turn delta).
- Stash only build-output directories (`detect::regenerable_dirs`). Never touch source files, `.git`, or anything outside the tree. `~/.gradle/caches` is out of scope.
- The `Stop` hook must return within milliseconds; it only records the turn end (spec, Lifecycle).
- No new file format outside `$BALE_HOME/store` and the record. No external `zstd` binary at runtime.
- Platform-neutral code; macOS is the only platform tested in this plan. `clonefile` use goes through `std::fs::copy`, which clones on APFS.
- Commits as Brandon McAnsh <git@bmcreations.dev>, no trailers.

## Review Focus

1. **A file rewritten while the tree was stashed** (a Codex thread in T3 has no pre-prompt hook and may build): unstash must keep the new file, not overwrite it with the stashed version. Test in Task 6.
2. **mtime drift:** if a restored file's mtime differs from the original, Xcode and Gradle rebuild it, turning a 1 s restore into a 45 s build. Test that mode and mtime round-trip exactly in Task 6.
3. **Crash between writing blobs and deleting files, or mid-restore:** a rerun must finish the job, and nothing may be deleted before its blob and the manifest are durable. Tests in Tasks 5 and 6.
4. **A busy tree** (build running, Xcode holding files): stash must refuse rather than delete files a process has open. Test in Task 5.
5. **GC deleting a blob that a patch still needs:** a base blob stays alive while any live blob patches against it. Test in Task 7.

---

### Task 1: zstd codec with patch-from

**Files:**
- Modify: `crates/bale-core/Cargo.toml` (add `zstd-safe = "7"`)
- Create: `crates/bale-core/src/store/codec.rs`, `crates/bale-core/src/store/mod.rs`
- Modify: `crates/bale-core/src/lib.rs` (add `pub mod store;`)
- Test: `crates/bale-core/tests/store_codec.rs`

**Interfaces:**
- Produces:
  - `pub fn compress(data: &[u8], level: i32) -> Result<Vec<u8>>`
  - `pub fn compress_patch(data: &[u8], base: &[u8], level: i32) -> Result<Vec<u8>>`
  - `pub fn decompress(frame: &[u8], base: Option<&[u8]>, size_hint: u64) -> Result<Vec<u8>>`
  - `pub const LEVEL: i32 = 3;` and `pub const PATCH_MAX_BYTES: u64 = 1 << 30;` (files larger than 1 GiB, or with a base larger than that, are stored without a base)

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn full_round_trip() {
    let data = sample(3 << 20, 7);
    let z = codec::compress(&data, codec::LEVEL).unwrap();
    assert!(z.len() < data.len());
    assert_eq!(codec::decompress(&z, None, data.len() as u64).unwrap(), data);
}

#[test]
fn patch_is_much_smaller_than_full_and_round_trips() {
    let base = sample(8 << 20, 1);
    let mut new = base.clone();
    new[4_000_000..4_000_100].fill(0xAB);
    let full = codec::compress(&new, codec::LEVEL).unwrap();
    let patch = codec::compress_patch(&new, &base, codec::LEVEL).unwrap();
    assert!(patch.len() * 20 < full.len(), "patch {} full {}", patch.len(), full.len());
    assert_eq!(codec::decompress(&patch, Some(&base), new.len() as u64).unwrap(), new);
}

#[test]
fn patch_with_wrong_base_fails_or_differs() {
    let base = sample(2 << 20, 1);
    let new = sample(2 << 20, 2);
    let patch = codec::compress_patch(&new, &base, codec::LEVEL).unwrap();
    let other = sample(2 << 20, 3);
    let out = codec::decompress(&patch, Some(&other), new.len() as u64);
    assert!(out.map(|o| o != new).unwrap_or(true));
}
```

`sample(len, seed)` is a test helper producing pseudo-random but compressible bytes (e.g. repeat a seeded 4 KiB random block with small mutations) so that `full` compresses but not to nothing.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p bale-core --test store_codec`
Expected: compile error, `store` module missing.

- [ ] **Step 3: Implement `codec.rs`**

Use `zstd_safe::CCtx` / `DCtx` directly; this is what `zstd --patch-from` does underneath. For `compress_patch`: set `CParameter::WindowLog(w)` where `w` = smallest power of two ≥ max(base.len(), data.len()), clamped to 10..=31; set `CParameter::EnableLongDistanceMatching(true)`; `cctx.ref_prefix(base)`; then `compress2`. For `decompress` with a base: set `DParameter::WindowLogMax(31)`, `dctx.ref_prefix(base)`, and decompress into a `Vec` with capacity `size_hint`. If `zstd-safe` 7 does not expose `ref_prefix` on both contexts, stop and report: the alternative is `CCtx::ref_cdict`-free `load_dictionary` with raw-content dictionaries, which needs a design check because a dictionary is limited in size.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p bale-core --test store_codec`
Expected: 3 passed.

- [ ] **Step 5: Commit** — `feat(store): add zstd codec with patch-from deltas`

---

### Task 2: Content-addressed blob store

**Files:**
- Create: `crates/bale-core/src/store/blob.rs`
- Modify: `crates/bale-core/src/store/mod.rs`
- Test: `crates/bale-core/tests/store_blob.rs`

**Interfaces:**
- Consumes: Task 1 codec.
- Produces:
  - `pub struct Store { root: PathBuf }` with `pub fn open(home: &BaleHome) -> Result<Store>` (root `$BALE_HOME/store`, creates `objects/` and `manifests/`)
  - `pub type Hash = String;` (lowercase hex SHA-256 of the *uncompressed* content)
  - `pub struct BlobMeta { pub hash: Hash, pub size: u64, pub base: Option<Hash>, pub depth: u32, pub stored: u64 }`
  - `pub fn put(&self, src: &Path, hash: &Hash, base: Option<&Hash>, max_depth: u32) -> Result<BlobMeta>`
  - `pub fn has(&self, hash: &Hash) -> bool`
  - `pub fn meta(&self, hash: &Hash) -> Result<BlobMeta>`
  - `pub fn read(&self, hash: &Hash) -> Result<Vec<u8>>` (resolves the base chain, verifies the SHA-256 of the result)
  - `pub fn remove(&self, hash: &Hash) -> Result<u64>` (bytes freed)
  - `pub fn list(&self) -> Result<Vec<BlobMeta>>`
  - `pub fn sha256_file(path: &Path) -> Result<Hash>` in `store/mod.rs`

Layout: `objects/<first 2 hex>/<hash>.zst` plus `objects/<first 2 hex>/<hash>.json` holding `BlobMeta`. Write both to a temp name in the same directory, `fsync`, then rename, `.zst` first. `has` checks the `.json`.

`put` rules, in order: if `has(hash)`, return the existing meta (dedup). Use the base only if it is given, present, its depth + 1 ≤ `max_depth`, and both sizes are ≤ `PATCH_MAX_BYTES`; depth is then base depth + 1, else 0 with no base. If the patch comes out no smaller than a full compression would be, the cost of comparing is not worth it: store the patch anyway (one compression per file).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn put_then_read_round_trips_and_dedups() // second put of same hash writes nothing new: object count unchanged
#[test] fn patch_chain_respects_max_depth()      // v1 full (depth 0), v2 patch on v1 (1), v3 on v2 (2), with max_depth 2 v4 on v3 is stored full (depth 0, base None)
#[test] fn read_detects_corruption()            // flip a byte in the .zst; read() returns Err mentioning the hash
#[test] fn missing_base_stores_full()           // put with base = Some(unknown hash) -> base None, depth 0
```

Assert exact `BlobMeta.depth` and `base` values as listed.

- [ ] **Step 2: Run to verify they fail** — `cargo test -p bale-core --test store_blob`, expect compile errors.
- [ ] **Step 3: Implement `blob.rs`** as specified above.
- [ ] **Step 4: Run to verify they pass** — expect 4 passed.
- [ ] **Step 5: Commit** — `feat(store): add content-addressed blob store`

---

### Task 3: Stash manifest, record field and config keys

**Files:**
- Create: `crates/bale-core/src/store/manifest.rs`
- Modify: `crates/bale-core/src/registry.rs` (field on `Record`), `crates/bale-core/src/config.rs`
- Test: `crates/bale-core/tests/store_manifest.rs`, extend `crates/bale-core/tests/` config test file if one exists, else in the same new file

**Interfaces:**
- Produces:
  - `pub struct Entry { pub rel: PathBuf, pub hash: Hash, pub size: u64, pub mode: u32, pub mtime_ns: i128 }` (`rel` is relative to the tree root)
  - `pub struct StashManifest { pub tree_id: String, pub created_unix: i64, pub entries: Vec<Entry> }`
  - `impl StashManifest { pub fn commitment(&self) -> Hash }` — SHA-256 over entries sorted by `rel`, each serialised as `rel\0hash\0size\0mode\0mtime_ns\n`
  - `Store::write_manifest(&self, m: &StashManifest) -> Result<Hash>` (file `manifests/<commitment>.json`, temp + fsync + rename), `Store::read_manifest(&self, c: &Hash) -> Result<StashManifest>` (verifies the commitment), `Store::list_manifests(&self) -> Result<Vec<Hash>>`, `Store::remove_manifest(&self, c: &Hash) -> Result<()>`
  - `Record.stash: Option<StashState>` with `pub struct StashState { pub commitment: Hash, pub at_unix: i64, pub restored: bool }`, `#[serde(default, skip_serializing_if = "Option::is_none")]` like the neighbouring fields. `restored: true` means the files are back but the manifest is kept as the patch base for the next stash.
  - `Config` gains `pub idle_stash: Duration` (key `idle_stash_minutes`, default 10), `pub stash_min_bytes: u64` (key `stash_min_bytes`, default 1048576), `pub patch_max_depth: u32` (key `patch_max_depth`, default 4).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn commitment_ignores_entry_order()      // same entries shuffled -> same commitment
#[test] fn commitment_changes_with_mtime()       // mtime_ns + 1 -> different commitment
#[test] fn read_manifest_rejects_tampering()     // edit a hash in the json file; read_manifest errs
#[test] fn old_records_without_stash_still_load() // a record json from main (no `stash` key) parses with stash None
#[test] fn config_defaults_and_repo_override()   // defaults 10 min / 1048576 / 4; `.bale.toml` with idle_stash_minutes = 3 -> 3 min
```

- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement** the types and functions above.
- [ ] **Step 4: Run to verify they pass**, then `cargo test -p bale-core` to confirm no existing record or config test broke.
- [ ] **Step 5: Commit** — `feat(store): add stash manifests, record state and config keys`

---

### Task 4: Stat index

Hashing all of a tree's large files on every stash would re-read ~6.4 GB per iOS tree. The index, like git's, lets bale hash only files whose stat changed.

**Files:**
- Create: `crates/bale-core/src/store/index.rs`
- Test: `crates/bale-core/tests/store_index.rs`

**Interfaces:**
- Produces:
  - `pub struct Index { entries: BTreeMap<PathBuf, IndexEntry> }`, `pub struct IndexEntry { pub size: u64, pub mtime_ns: i128, pub ino: u64, pub hash: Hash }`
  - `Index::load(store: &Store, tree_id: &str) -> Result<Index>` (missing file = empty), `Index::save(&self, store: &Store, tree_id: &str) -> Result<()>` (file `index/<tree_id>.json`)
  - `Index::hash(&mut self, root: &Path, rel: &Path) -> Result<Hash>` — returns the cached hash when size, mtime_ns and ino all match, otherwise hashes the file and updates the entry
  - `Index::forget_missing(&mut self, keep: &BTreeSet<PathBuf>)`

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn unchanged_file_is_not_rehashed()  // hash once; replace contents keeping size and restoring mtime via filetime, same inode -> cached (old) hash returned. This pins that stat is trusted, as in git.
#[test] fn changed_mtime_rehashes()          // touch the file -> new hash computed (equals sha256_file)
#[test] fn index_survives_save_and_load()
```

Add `filetime = "0.2"` as a dev-dependency (and as a normal dependency for Task 6, which sets mtimes).

- [ ] **Step 2–4:** fail, implement, pass.
- [ ] **Step 5: Commit** — `feat(store): add stat index to skip rehashing unchanged files`

---

### Task 5: `stash` operation

**Files:**
- Create: `crates/bale-core/src/ops/stash.rs`
- Modify: `crates/bale-core/src/ops/mod.rs`
- Test: `crates/bale-core/tests/stash.rs`

**Interfaces:**
- Consumes: `Store`, `Index`, `StashManifest`, `Record.stash`, `Config.{stash_min_bytes, patch_max_depth}`, `detect::{detect, regenerable_dirs}`, `busy::busy_holders_many`, `lock::WorkspaceLock::try_acquire`, `par::par_map`.
- Produces:
  - `pub struct StashReport { pub files: usize, pub bytes: u64, pub stored_bytes: u64, pub reused: usize, pub secs: f64 }`
  - `pub enum StashOutcome { Stashed(StashReport), Skipped(String) }`
  - `pub fn stash(home: &BaleHome, id: &str, ignore_pids: &[u32]) -> Result<StashOutcome>`

Steps of `stash`, in order:
1. Take the workspace lock; skip with a reason if held. Skip if the record's level is not `Hot` or `Baled`, or if `stash` is `Some` with `restored: false` (already stashed).
2. Build dirs = `regenerable_dirs(root, detect(root), cfg)`. Skip with `"busy: <holders>"` if `busy_holders_many(dirs, ignore_pids)` finds any holder.
3. Candidates: regular files (not symlinks, `nlink == 1`) under those dirs with size ≥ `stash_min_bytes`. Leave files that carry extended attributes in place (count them in the report's skipped reason only in debug output); this avoids losing xattrs we do not restore.
4. Base lookup: if `stash` is `Some(restored: true)`, read that manifest and map `rel -> hash`; that hash is the patch base for the same `rel`.
5. Hash candidates through the index (`par_map`, 8-way), then `put` each (in parallel); count `reused` when the blob already existed.
6. Write the manifest, set `record.stash = Some(StashState { commitment, at_unix: now, restored: false })`, save the record. Only now:
7. Delete each candidate whose current stat still matches the index entry (a file that changed during steps 5–6 stays). Save the index.
8. Remove the previous manifest if it was a `restored: true` base (its blobs stay until GC decides).

- [ ] **Step 1: Write the failing tests** (use a temp `BaleHome` and a fixture tree with a `.dd` dir detected as Xcode output, like the existing `tests/up.rs` fixtures)

```rust
#[test] fn stashes_only_large_build_files()          // 2 MiB + 3 MiB files in .dd are gone; 4 KiB file in .dd and a 5 MiB source file outside build dirs remain; report.files == 2
#[test] fn second_stash_patches_against_first()      // stash, unstash (Task 6 not yet: simulate by writing files back and setting restored: true), modify 100 bytes of one file, stash again -> that blob's meta.base == old hash, depth 1
#[test] fn refuses_busy_tree()                       // hold a .dd file open in a child process (`sleep` with redirect, as tests/cold.rs does) -> Skipped("busy: ...") and all files present
#[test] fn crash_before_delete_leaves_files_and_rerun_completes() // inject failure after manifest write via a test hook (cfg(test) env var BALE_TEST_FAIL_AT=stash-delete) -> files still present, record.stash set; rerun stash() returns Stashed and files are gone
#[test] fn file_changed_during_stash_is_kept()       // use BALE_TEST_FAIL_AT-style pause hook or modify between index and delete via a test-only callback -> that file remains
```

- [ ] **Step 2–4:** fail, implement, pass. Then `cargo test -p bale-core`.
- [ ] **Step 5: Commit** — `feat(core): add stash for large build-output files`

Note on step 1 of `crash_before_delete...`: a rerun with `stash` already `Some(restored: false)` must not skip; it re-deletes whatever manifest entries are still present and unchanged. Adjust rule 1 accordingly: skip only when no manifest entry is still present on disk.

---

### Task 6: `unstash` operation

**Files:**
- Create: `crates/bale-core/src/ops/unstash.rs`
- Modify: `crates/bale-core/src/ops/mod.rs`
- Test: `crates/bale-core/tests/unstash.rs`

**Interfaces:**
- Consumes: Task 2–5 types.
- Produces:
  - `pub struct UnstashReport { pub files: usize, pub bytes: u64, pub kept_newer: usize, pub secs: f64 }`
  - `pub fn unstash(home: &BaleHome, id: &str) -> Result<Option<UnstashReport>>` (`None` when the tree is not stashed; must cost one record read in that case, since every prompt calls it)
  - `pub fn unstash_path(home: &BaleHome, cwd: &Path) -> Result<Option<UnstashReport>>` — maps a path inside a tree to its record (longest registered `path` prefix) and calls `unstash`

Restore rules:
- Group entries by hash. For each hash, decompress once into `<dir>/.bale-tmp-<rand>`, verify SHA-256, set mode and mtime (`filetime::set_file_mtime` with nanosecond precision), rename into place. Further entries with the same hash are `std::fs::copy` of the first restored file (an APFS clone) followed by the same mode/mtime set.
- If a file already exists at `rel`, leave it, count `kept_newer`.
- Run groups in parallel, 8 at a time.
- On success set `record.stash.restored = true` and save. A failure mid-way leaves `restored: false`; a rerun restores the rest (present files are skipped by the rule above).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn round_trip_restores_bytes_mode_and_mtime() // after stash+unstash, every file's bytes, permissions and mtime (ns) equal the pre-stash values
#[test] fn duplicate_content_is_restored_once_and_cloned() // two identical 2 MiB files in different dirs -> both restored, store read once (count via a test counter or by checking report)
#[test] fn newer_file_at_path_is_kept()               // after stash, write a new file at one stashed path -> unstash keeps its content; kept_newer == 1
#[test] fn rerun_after_partial_restore_completes()    // BALE_TEST_FAIL_AT=unstash-mid after first group -> Err; rerun -> Ok, all files present and correct
#[test] fn not_stashed_is_none()                      // unstash on a hot tree returns Ok(None)
#[test] fn unstash_path_finds_tree_from_subdir()
```

- [ ] **Step 2–4:** fail, implement, pass.
- [ ] **Step 5: Commit** — `feat(core): add unstash with hash check and mtime restore`

---

### Task 7: Store garbage collection

**Files:**
- Create: `crates/bale-core/src/store/gc.rs`
- Modify: `crates/bale-core/src/ops/gc.rs` (call it and add `store_bytes_freed: u64` to `GcReport`), `crates/bale-cli/src/main.rs` (print it with the existing gc output)
- Test: `crates/bale-core/tests/store_gc.rs`

**Interfaces:**
- Produces: `pub fn collect(store: &Store, live_manifests: &[Hash], dry_run: bool) -> Result<u64>`

Live manifests are the `stash.commitment` of every record. Live blobs = every entry hash in a live manifest, plus, transitively, the `base` of every live blob. Remove every other blob and every manifest not in the live set. Also remove `index/<id>.json` for ids with no record.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn keeps_base_of_live_patch()      // manifest M2 references v2 (patch on v1); v1 not in any manifest -> v1 kept
#[test] fn removes_unreferenced_blobs_and_manifests() // after record deleted, all its blobs go; returned bytes == sum of stored sizes
#[test] fn dry_run_removes_nothing()
```

- [ ] **Step 2–4:** fail, implement, pass; `cargo test -p bale-core --test new_gc` still passes.
- [ ] **Step 5: Commit** — `feat(store): collect unreferenced blobs in bale gc`

---

### Task 8: Sweep stashes idle trees

**Files:**
- Modify: `crates/bale-core/src/ops/sweep.rs`, `crates/bale-core/src/trace.rs`
- Test: `crates/bale-core/tests/sweep.rs`

**Interfaces:**
- Consumes: `stash`, `Config.idle_stash`.
- Produces: `Action::Stash` variant; `decide` gains a `stashed: bool` parameter.

Policy changes in `decide` (keep the existing order: cold first):
- `Hot` or `Baled`, not stashed, `idle_for >= cfg.idle_stash` → `Action::Stash`, unless a cold or up action applies first.
- A stashed tree never gets `Action::Up`: stashing replaces deleting build output. It still goes cold at `idle_cold`; cold must unstash nothing, and Task 5's manifest stays until GC (cold's existing removal of build dirs deletes the small files; the stashed blobs are dropped by GC once the cold record's stash is cleared — clear `record.stash` in `cold` when it removes the build dirs).
- Trace: add a `stash` line `{id, files, bytes, stored_bytes, secs}` and an `unstash` line with the same fields plus `kept_newer`, so the replay can measure real restore times.

- [ ] **Step 1: Write the failing tests** in `tests/sweep.rs`

```rust
#[test] fn decide_stashes_after_idle_stash()      // Hot, 11 min, clean -> Stash; 9 min -> Keep
#[test] fn decide_never_ups_a_stashed_tree()      // Baled, has_regen, stashed, 5 h -> Keep
#[test] fn decide_cold_beats_stash()              // idle 8 days clean -> Cold
#[test] fn sweep_stashes_and_traces()             // fixture tree idle 11 min -> files stashed, trace has one `stash` line
```

- [ ] **Step 2–4:** fail, implement, pass; update existing `decide` call sites and tests for the new parameter.
- [ ] **Step 5: Commit** — `feat(sweep): stash trees idle past idle_stash`

The launchd sweep runs every 30 minutes, so T = 10 minutes is only met at 30-minute granularity. Change the launchd interval written by `bale install` to 5 minutes in this task (find it with `grep -rn StartInterval crates/`), and note it in the commit message.

---

### Task 9: CLI commands and Claude Code hooks

**Files:**
- Modify: `crates/bale-cli/src/main.rs` (subcommands `stash <target> [--ignore-pid N]`, `unstash <target>`; hook kinds), `crates/bale-hooks/src/claude.rs` (events and handlers), `crates/bale-hooks/src/lib.rs` if it dispatches hooks
- Test: `crates/bale-cli/tests/cli.rs`, `crates/bale-hooks` existing claude tests

**Interfaces:**
- Consumes: `stash`, `unstash_path`.
- Produces:
  - `bale stash <target>` / `bale unstash <target>` print one line: `stashed 612 files, 6.4 GB -> 1.1 GB in 21.3 s` / `restored 612 files, 6.4 GB in 1.8 s (1 kept newer)`.
  - `bale hook claude prompt` (for `UserPromptSubmit`) and `bale hook claude pre-tool` (for `PreToolUse` with matcher `Bash`): read the hook JSON from stdin, take `cwd`, call `unstash_path`. Exit 0 on success or when `cwd` is in no tree. On any error, including a hash mismatch, exit 2 with the reason on stderr, so Claude Code blocks the prompt and shows it (spec, Lifecycle step 3: never continue silently). The store and manifest are left as they are.
  - The spec says no work on `PreToolUse`. This plan keeps one hook there, limited to `Bash`, because a tree can be stashed during a permission wait longer than T, after `UserPromptSubmit` has already run. On a tree that isn't stashed it costs one record read.
  - `bale hook claude stop` (for `Stop`): update `last_activity` of the tree at `cwd` and exit. No hashing, no store access.
  - `HOOKS` in `claude.rs` grows to five entries: the existing two plus `("UserPromptSubmit", "prompt")`, `("PreToolUse", "pre-tool")` with matcher `"Bash"`, `("Stop", "stop")`. Install and uninstall plans handle the matcher; uninstall removes only entries bale added (existing behaviour).

- [ ] **Step 1: Write the failing tests**

```rust
// cli.rs
#[test] fn stash_and_unstash_commands_round_trip()   // temp home + fixture tree: `bale stash <path>` then `bale unstash <path>`; stdout matches ^stashed \d+ files and ^restored \d+ files
#[test] fn hook_prompt_unstashes_tree_at_cwd()       // stash fixture; pipe {"cwd": "<tree>/sub", "hook_event_name": "UserPromptSubmit"} to `bale hook claude prompt` -> exit 0, files back
#[test] fn hook_prompt_outside_any_tree_is_noop()    // cwd=/tmp -> exit 0, no output on stdout
#[test] fn hook_prompt_fails_loudly_on_corrupt_blob() // stash, flip a byte in one .zst -> exit 2, stderr names the file, store and manifest unchanged
#[test] fn hook_stop_only_touches_activity()         // record last_activity advances; store directory untouched (mtime of store root unchanged)
// claude.rs tests
#[test] fn install_plan_adds_prompt_pretool_stop()   // settings JSON gains the three events, PreToolUse with matcher "Bash"; existing user hooks kept
#[test] fn uninstall_removes_only_bale_entries()
```

- [ ] **Step 2–4:** fail, implement, pass; full `cargo test --workspace`.
- [ ] **Step 5: Commit** — `feat(hooks): unstash before Claude Code prompts and Bash calls`

---

### Task 10: Real-tree acceptance run

No code; this checks the plan against the restore budget on real build output before anything is installed live.

**Files:**
- Create: `docs/bench/2026-10-warm-tier.md` (results), script under the session scratchpad, not in the repo

- [ ] **Step 1:** `cargo build --release`. Using a temp `BALE_HOME` (never `~/.bale`, never `~/.local/bin/bale`), `git clone --shared` the Flipcash iOS repo into a scratch dir, copy its gitignored `Configurations/secrets.local.xcconfig`, build once with `-derivedDataPath <clone>/.dd CODE_SIGNING_ALLOWED=NO`, and `bale adopt` it.
- [ ] **Step 2:** Run `bale stash` then `bale unstash` 5 times; record stash and restore seconds, stored bytes, and disk freed (`du -sk .dd` before and after stash).
- [ ] **Step 3:** Make the leaf edit from the spec's calibration (`Flipcash/UI/EnterAmountView.swift` string), incremental build, stash, unstash; record times and stored bytes (expect tens of MB from patching).
- [ ] **Step 4:** After unstash, run the incremental build again with no edit and record its time. **Expected:** about the same as a no-op build before stashing (seconds). A full rebuild means mtimes or content did not round-trip: stop and report.
- [ ] **Step 5:** Write the numbers to `docs/bench/2026-10-warm-tier.md`, delete the clone and the temp home, commit — `docs: add warm tier acceptance results`.

Pass criteria: restore p50 ≤ 2 s and worst of 5 ≤ 5 s on the full stash; post-unstash no-op build not slower than before stashing.
