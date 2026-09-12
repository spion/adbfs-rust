# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

adbfs-rootless is a FUSE filesystem that mounts Android devices over ADB without requiring root. The main branch is a Rust port using fuser (FUSE3); the legacy C++ version uses libfuse (FUSE2) and is part of the commit history

## Build & Test Commands

```bash
cargo build                    # debug build
cargo build --release          # release binary → target/release/adbfs
cargo test                     # all unit + property tests
cargo test parse::tests        # single module
cargo fmt --check              # formatting check
cargo clippy -- -D warnings    # lint (CI enforces this)
```

System dependency: `libfuse3-dev` (or equivalent for your distro).

## Running

```bash
./target/release/adbfs ~/droid                     # basic mount
./target/release/adbfs --cache-ttl 60 --rescan ~/droid  # custom TTL + media rescan
./target/release/adbfs -d ~/droid                  # foreground + debug logging
./target/release/adbfs -o uid=1000,umask=022 ~/droid    # libfuse ownership overrides
fusermount -u ~/droid                              # unmount
```

## Architecture

Three-layer design:

```
FUSE interface (fs.rs)
  ↓ inode ↔ path mapping, file handle management, metadata cache
Device operations (ops.rs)
  ↓ high-level ops: list_dir, get_metadata, pull, push, mkdir, rm, mv, touch
ADB transport (adb/)
  ↓ shell commands via std::process::Command
Parsing & escaping (parse.rs, escape.rs)
```

### Key design decisions

- **Inode mapping**: ADB is path-based, FUSE is inode-based. `fs.rs` maintains bidirectional `DashMap<String, u64>` / `DashMap<u64, String>` with atomic inode counter.

- **Readdir caching**: FUSE3's `reply.add()` returns true when buffer is full, then the kernel calls readdir again with a new offset. Directory listings are fetched once in `opendir()` and cached per file-handle, then served from cache in `readdir()`. Without this, every offset continuation re-executes the adb `ls` command (this was the root cause of a 10x slowdown vs the C++ version).

- **Backgrounds by default**: libfuse2's `fuse_main` daemonized on its own, and `tests/run.sh` relies on that. fuser has no equivalent, so `main.rs` forks explicitly. The fork sits between `Session::new` (mount + kernel handshake) and `Session::spawn` (request loop): mounting first means a failed mount still exits non-zero in the foreground, and forking before any thread exists means the worker pool survives. `-f` keeps it in the foreground.

- **File I/O via pull/push**: `open()` pulls the device file to a local tempdir. Writes go to the local copy. `flush()` pushes it back and syncs.

- **Blocking transport**: `AdbDevice` is a plain blocking trait. Each ADB command runs on the FUSE worker thread that issued it, and concurrency comes from the FUSE session's thread pool (`config.n_threads`, set to `available_parallelism()` in `fs.rs`). There is no async runtime: every call site was `block_on` on a worker thread, so tokio bought nothing over `std::process::Command`.

- **libfuse high-level options**: `uid`, `gid`, `umask`, the three timeouts, `direct_io`, `kernel_cache` and `debug` were implemented by libfuse's high-level layer, which fuser does not have. `mount_opts.rs` parses them out of `-o` before mounting — `fusermount3` fails the mount on any option it does not recognise, so they cannot simply be forwarded. Everything it does not claim is forwarded unchanged. `fuser::ReplyEntry::entry` takes a single TTL for both the name and the attributes, so `entry_timeout` governs entry replies and `attr_timeout` only `reply.attr`; libfuse could set the two independently.

### Module roles

| Module       | Role                                                                           |
| ------------ | ------------------------------------------------------------------------------ |
| `fs.rs`      | fuser::Filesystem impl — all FUSE callbacks, inode/handle maps, metadata cache |
| `ops.rs`     | DeviceOps — high-level device operations, symlink resolution, media rescan     |
| `cache.rs`   | MetadataCache — DashMap with TTL expiration, prefix invalidation               |
| `adb/mod.rs` | AdbDevice trait definition                                                     |
| `adb/cli.rs` | AdbCli — concrete impl executing `adb` binary                                  |
| `parse.rs`   | Parses Android `ls -l` output (multiple formats), mode strings, symlinks       |
| `mount_opts.rs` | Parses libfuse high-level `-o` options, splitting them from kernel options |
| `escape.rs`  | Shell escaping for adb shell commands and paths                                |

## Testing

- **Property tests** in `parse.rs` and `escape.rs` (proptest) — verify parser robustness and escaping invariants
- **Mock ADB** in `ops.rs` — `MockAdb` implements `AdbDevice` for testing without a device
- **Integration tests** in CI — Android emulator (API 29) via `tests/run.sh`

## CI

GitHub Actions (`.github/workflows/rust.yml`): fmt → clippy → build → test → release build → integration test on Android emulator.
