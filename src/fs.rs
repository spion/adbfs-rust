use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use fuser::{
  AccessFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
  INodeNo, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
  ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, Session, SessionACL,
  TimeOrNow, WriteFlags,
};
use tracing::{debug, trace, warn};

use crate::cache::MetadataCache;
use crate::mount_opts::FsOptions;
use crate::ops::{DeviceError, DeviceOps};
use crate::parse::FileMeta;

const FUSE_ROOT_INO: u64 = 1;

fn err(code: i32) -> Errno {
  Errno::from_i32(code)
}

fn device_err(e: DeviceError) -> Errno {
  err(e.to_errno())
}

fn io_err(e: std::io::Error) -> Errno {
  Errno::from(e)
}

/// The local copy of one device file, shared by every handle open on that
/// inode. It is pulled once, on the first open, and deleted when the last
/// handle is released. Sharing it is what makes two concurrent opens behave
/// like two descriptors on one file instead of two copies that overwrite each
/// other on push.
struct LocalCopy {
  path: PathBuf,
  /// Written locally but not yet pushed. Outside the mutex so `write` stays
  /// lock-free on the hot path.
  dirty: AtomicBool,
  state: Mutex<CopyState>,
}

struct CopyState {
  refs: usize,
  /// The local file holds the device's contents.
  pulled: bool,
  /// The last handle went away and the local file is gone. A thread that was
  /// waiting on the mutex must start over with a fresh copy.
  evicted: bool,
}

/// How `acquire_copy` should populate a copy it had to create.
enum CopyInit {
  /// Pull the device's contents.
  Pull,
  /// Start from an empty file, for `create` and truncate-to-zero.
  Empty,
}

impl LocalCopy {
  fn new(path: PathBuf) -> Self {
    Self {
      path,
      dirty: AtomicBool::new(false),
      state: Mutex::new(CopyState {
        refs: 0,
        pulled: false,
        evicted: false,
      }),
    }
  }
}

struct OpenFile {
  ino: u64,
  copy: Arc<LocalCopy>,
  file: std::fs::File,
}

struct OpenDir {
  entries: Vec<(u64, FileType, String)>,
}

pub struct AdbFs {
  ops: Arc<DeviceOps>,
  cache: MetadataCache,
  open_files: DashMap<u64, OpenFile>,
  open_copies: DashMap<u64, Arc<LocalCopy>>,
  open_dirs: DashMap<u64, OpenDir>,
  tmp_dir: tempfile::TempDir,
  opts: FsOptions,
  next_fh: AtomicU64,
  // Inode mapping: ADB is path-based, FUSE is inode-based
  path_to_ino: DashMap<String, u64>,
  ino_to_path: DashMap<u64, String>,
  next_ino: AtomicU64,
}

impl AdbFs {
  pub fn new(
    ops: Arc<DeviceOps>,
    cache_ttl: Duration,
    opts: FsOptions,
  ) -> color_eyre::Result<Self> {
    let fs = Self {
      ops,
      cache: MetadataCache::new(cache_ttl),
      open_files: DashMap::new(),
      open_copies: DashMap::new(),
      open_dirs: DashMap::new(),
      tmp_dir: tempfile::TempDir::new()?,
      opts,
      next_fh: AtomicU64::new(1),
      path_to_ino: DashMap::new(),
      ino_to_path: DashMap::new(),
      next_ino: AtomicU64::new(FUSE_ROOT_INO + 1),
    };
    fs.path_to_ino.insert("/".to_string(), FUSE_ROOT_INO);
    fs.ino_to_path.insert(FUSE_ROOT_INO, "/".to_string());
    Ok(fs)
  }

  fn get_or_assign_ino(&self, path: &str) -> u64 {
    if let Some(ino) = self.path_to_ino.get(path) {
      return *ino;
    }
    let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
    self.path_to_ino.insert(path.to_string(), ino);
    self.ino_to_path.insert(ino, path.to_string());
    ino
  }

  fn get_path(&self, ino: u64) -> Option<String> {
    self.ino_to_path.get(&ino).map(|v| v.clone())
  }

  /// Moves the inode mapping for `from`, and for everything below it, to `to`.
  ///
  /// The kernel keeps its dentry across a rename, so the inode it holds has to
  /// resolve to the new path or every later call on the renamed file fails.
  fn rekey_paths(&self, from: &str, to: &str) {
    let dir = format!("{from}/");
    let moved: Vec<(String, u64)> = self
      .path_to_ino
      .iter()
      .filter(|e| e.key() == from || e.key().starts_with(&dir))
      .map(|e| (e.key().clone(), *e.value()))
      .collect();

    for (old, ino) in moved {
      let new = match old.strip_prefix(&dir) {
        Some(rest) => format!("{to}/{rest}"),
        None => to.to_string(),
      };
      self.path_to_ino.remove(&old);
      // Renaming onto an existing file leaves that file's inode unreachable,
      // which is what the kernel expects: it is gone.
      if let Some((_, replaced)) = self.path_to_ino.remove(&new) {
        self.ino_to_path.remove(&replaced);
      }
      self.path_to_ino.insert(new.clone(), ino);
      self.ino_to_path.insert(ino, new);
    }
  }

  fn child_path(parent: &str, name: &OsStr) -> String {
    let name_str = name.to_string_lossy();
    if parent == "/" {
      format!("/{name_str}")
    } else {
      format!("{parent}/{name_str}")
    }
  }

  fn meta_to_attr(&self, ino: u64, meta: &FileMeta) -> FileAttr {
    let mut uid = meta.uid;
    let mut gid = meta.gid;
    let mode = self.opts.apply_ownership(meta.mode, &mut uid, &mut gid);
    FileAttr {
      ino: INodeNo(ino),
      size: meta.size,
      blocks: meta.size.div_ceil(512),
      atime: meta.mtime,
      mtime: meta.mtime,
      ctime: meta.mtime,
      crtime: UNIX_EPOCH,
      kind: mode_to_filetype(mode),
      perm: (mode & 0o7777) as u16,
      nlink: meta.nlink,
      uid,
      gid,
      rdev: meta.rdev as u32,
      blksize: 512,
      flags: 0,
    }
  }

  /// Cache hints for an open file handle, from `-o direct_io` / `-o kernel_cache`.
  fn file_open_flags(&self) -> FopenFlags {
    let mut flags = FopenFlags::empty();
    flags.set(FopenFlags::FOPEN_DIRECT_IO, self.opts.direct_io);
    flags.set(FopenFlags::FOPEN_KEEP_CACHE, self.opts.kernel_cache);
    flags
  }

  fn push_and_sync(&self, local: &std::path::Path, device_path: &str) -> Result<(), DeviceError> {
    self.ops.push(local, device_path)?;
    self.ops.sync_device()
  }

  /// The local file is named after the inode, not the device path: unique
  /// whatever the path contains, and never longer than `NAME_MAX`.
  fn local_path_for(&self, ino: u64) -> PathBuf {
    self.tmp_dir.path().join(ino.to_string())
  }

  /// Takes a reference on `ino`'s local copy, creating and populating it if
  /// this is the first reference. Every caller must pair this with
  /// [`Self::release_copy`].
  fn acquire_copy(&self, ino: u64, path: &str, init: CopyInit) -> Result<Arc<LocalCopy>, Errno> {
    loop {
      let copy = {
        let entry = self
          .open_copies
          .entry(ino)
          .or_insert_with(|| Arc::new(LocalCopy::new(self.local_path_for(ino))));
        Arc::clone(&entry)
      };

      // Held across the pull so that concurrent opens of one file pull once.
      let mut state = copy.state.lock().expect("local copy mutex poisoned");
      if state.evicted {
        // The last handle was released while we waited. Start over: this copy
        // is no longer in the map and its local file is gone.
        continue;
      }

      let populated = match init {
        CopyInit::Empty => std::fs::File::create(&copy.path)
          .map(|_| copy.dirty.store(true, Ordering::Release))
          .map_err(io_err),
        CopyInit::Pull if !state.pulled => self.ops.pull(path, &copy.path).map_err(device_err),
        CopyInit::Pull => Ok(()),
      };

      if let Err(e) = populated {
        if state.refs == 0 {
          state.evicted = true;
          self
            .open_copies
            .remove_if(&ino, |_, v| Arc::ptr_eq(v, &copy));
        }
        return Err(e);
      }

      state.pulled = true;
      state.refs += 1;
      drop(state);
      return Ok(copy);
    }
  }

  /// Drops a reference taken by [`Self::acquire_copy`], deleting the local file
  /// once no handle is left.
  fn release_copy(&self, ino: u64, copy: &Arc<LocalCopy>) {
    let mut state = copy.state.lock().expect("local copy mutex poisoned");
    state.refs -= 1;
    if state.refs == 0 {
      state.evicted = true;
      let _ = std::fs::remove_file(&copy.path);
      self
        .open_copies
        .remove_if(&ino, |_, v| Arc::ptr_eq(v, copy));
    }
  }

  /// Pushes the local copy back to the device if it has unpushed writes.
  fn push_if_dirty(&self, ino: u64, copy: &LocalCopy) -> Result<(), Errno> {
    // Cleared first: a write that races with the push re-marks the copy dirty
    // rather than having its data dropped by the store afterwards.
    if !copy.dirty.swap(false, Ordering::AcqRel) {
      return Ok(());
    }
    let Some(path) = self.get_path(ino) else {
      copy.dirty.store(true, Ordering::Release);
      return Err(err(libc::ENOENT));
    };
    if let Err(e) = self.push_and_sync(&copy.path, &path) {
      copy.dirty.store(true, Ordering::Release);
      return Err(device_err(e));
    }
    self.cache.invalidate(&path);
    Ok(())
  }

  fn fetch_meta(&self, path: &str) -> Result<FileMeta, DeviceError> {
    match self.cache.get(path) {
      Some(Some(meta)) => {
        trace!(path = %path, "cache hit");
        return Ok(meta);
      }
      Some(None) => {
        trace!(path = %path, "negative cache hit");
        return Err(DeviceError::NotFound {
          path: path.to_string(),
        });
      }
      None => {}
    }
    match self.ops.get_metadata(path) {
      Ok(meta) => {
        self.cache.insert(path.to_string(), Some(meta.clone()));
        Ok(meta)
      }
      Err(e @ DeviceError::NotFound { .. }) => {
        self.cache.insert(path.to_string(), None);
        Err(e)
      }
      Err(e) => Err(e),
    }
  }
}

/// FUSE reports a cacheable miss as an entry reply with inode zero. The kernel
/// reads nothing else from it, so every other field is left empty.
const NEGATIVE_ENTRY: FileAttr = FileAttr {
  ino: INodeNo(0),
  size: 0,
  blocks: 0,
  atime: UNIX_EPOCH,
  mtime: UNIX_EPOCH,
  ctime: UNIX_EPOCH,
  crtime: UNIX_EPOCH,
  kind: FileType::RegularFile,
  perm: 0,
  nlink: 0,
  uid: 0,
  gid: 0,
  rdev: 0,
  blksize: 0,
  flags: 0,
};

fn mode_to_filetype(mode: u32) -> FileType {
  let ft = mode & libc::S_IFMT;
  match ft {
    x if x == libc::S_IFDIR => FileType::Directory,
    x if x == libc::S_IFLNK => FileType::Symlink,
    x if x == libc::S_IFBLK => FileType::BlockDevice,
    x if x == libc::S_IFCHR => FileType::CharDevice,
    x if x == libc::S_IFIFO => FileType::NamedPipe,
    x if x == libc::S_IFSOCK => FileType::Socket,
    _ => FileType::RegularFile,
  }
}

impl Filesystem for AdbFs {
  fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
    let ino_raw = ino.0;
    trace!(ino = ino_raw, "getattr");
    let path = match self.get_path(ino_raw) {
      Some(p) => p,
      None => {
        reply.error(err(libc::ENOENT));
        return;
      }
    };

    match self.fetch_meta(&path) {
      Ok(meta) => {
        let attr = self.meta_to_attr(ino_raw, &meta);
        reply.attr(&self.opts.attr_timeout, &attr);
      }
      Err(e) => reply.error(device_err(e)),
    }
  }

  fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
    let parent_raw = parent.0;
    let parent_path = match self.get_path(parent_raw) {
      Some(p) => p,
      None => {
        reply.error(err(libc::ENOENT));
        return;
      }
    };
    let full_path = Self::child_path(&parent_path, name);
    trace!(path = %full_path, "lookup");

    match self.fetch_meta(&full_path) {
      Ok(meta) => {
        let ino = self.get_or_assign_ino(&full_path);
        let attr = self.meta_to_attr(ino, &meta);
        reply.entry(&self.opts.entry_timeout, &attr, Generation(0));
      }
      Err(DeviceError::NotFound { .. }) if !self.opts.negative_timeout.is_zero() => {
        reply.entry(&self.opts.negative_timeout, &NEGATIVE_ENTRY, Generation(0));
      }
      Err(e) => reply.error(device_err(e)),
    }
  }

  fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
    let ino_raw = ino.0;
    let path = match self.get_path(ino_raw) {
      Some(p) => p,
      None => {
        reply.error(err(libc::ENOENT));
        return;
      }
    };
    trace!(path = %path, "opendir");

    let entries = match self.ops.list_dir(&path) {
      Ok(e) => e,
      Err(e) => {
        reply.error(device_err(e));
        return;
      }
    };

    let mut full_entries: Vec<(u64, FileType, String)> = vec![
      (ino_raw, FileType::Directory, ".".to_string()),
      (ino_raw, FileType::Directory, "..".to_string()),
    ];

    for (name, meta) in entries {
      if name == "." || name == ".." {
        continue;
      }
      let child_path = Self::child_path(&path, OsStr::new(&name));
      let child_ino = self.get_or_assign_ino(&child_path);
      let ft = meta
        .as_ref()
        .map(|m| mode_to_filetype(m.mode))
        .unwrap_or(FileType::RegularFile);
      if let Some(m) = meta {
        self.cache.insert(child_path, Some(m));
      }
      full_entries.push((child_ino, ft, name));
    }

    let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
    self.open_dirs.insert(
      fh,
      OpenDir {
        entries: full_entries,
      },
    );
    reply.opened(FileHandle(fh), FopenFlags::empty());
  }

  fn readdir(
    &self,
    _req: &Request,
    _ino: INodeNo,
    fh: FileHandle,
    offset: u64,
    mut reply: ReplyDirectory,
  ) {
    let dir = match self.open_dirs.get(&fh.0) {
      Some(d) => d,
      None => {
        reply.error(err(libc::EBADF));
        return;
      }
    };

    for (i, (child_ino, ft, name)) in dir.entries.iter().enumerate().skip(offset as usize) {
      if reply.add(INodeNo(*child_ino), (i + 1) as u64, *ft, name) {
        break;
      }
    }
    reply.ok();
  }

  fn releasedir(
    &self,
    _req: &Request,
    _ino: INodeNo,
    fh: FileHandle,
    _flags: OpenFlags,
    reply: ReplyEmpty,
  ) {
    self.open_dirs.remove(&fh.0);
    reply.ok();
  }

  fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
    let ino_raw = ino.0;
    let path = match self.get_path(ino_raw) {
      Some(p) => p,
      None => {
        reply.error(err(libc::ENOENT));
        return;
      }
    };
    trace!(path = %path, "open");

    let copy = match self.acquire_copy(ino_raw, &path, CopyInit::Pull) {
      Ok(c) => c,
      Err(e) => {
        warn!(path = %path, errno = ?e, "pull failed");
        reply.error(e);
        return;
      }
    };

    let file = match std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .open(&copy.path)
    {
      Ok(f) => f,
      Err(e) => {
        self.release_copy(ino_raw, &copy);
        reply.error(io_err(e));
        return;
      }
    };

    let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
    self.open_files.insert(
      fh,
      OpenFile {
        ino: ino_raw,
        copy,
        file,
      },
    );
    reply.opened(FileHandle(fh), self.file_open_flags());
  }

  fn read(
    &self,
    _req: &Request,
    _ino: INodeNo,
    fh: FileHandle,
    offset: u64,
    size: u32,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    reply: ReplyData,
  ) {
    let fh_raw = fh.0;
    let mut entry = match self.open_files.get_mut(&fh_raw) {
      Some(e) => e,
      None => {
        reply.error(err(libc::EBADF));
        return;
      }
    };
    let mut buf = vec![0u8; size as usize];
    match entry
      .file
      .seek(SeekFrom::Start(offset))
      .and_then(|_| entry.file.read(&mut buf))
    {
      Ok(n) => reply.data(&buf[..n]),
      Err(e) => reply.error(io_err(e)),
    }
  }

  fn write(
    &self,
    _req: &Request,
    _ino: INodeNo,
    fh: FileHandle,
    offset: u64,
    data: &[u8],
    _write_flags: WriteFlags,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    reply: ReplyWrite,
  ) {
    let fh_raw = fh.0;
    let mut entry = match self.open_files.get_mut(&fh_raw) {
      Some(e) => e,
      None => {
        reply.error(err(libc::EBADF));
        return;
      }
    };
    match entry
      .file
      .seek(SeekFrom::Start(offset))
      .and_then(|_| entry.file.write(data))
    {
      Ok(n) => {
        entry.copy.dirty.store(true, Ordering::Release);
        reply.written(n as u32);
      }
      Err(e) => reply.error(io_err(e)),
    }
  }

  fn flush(
    &self,
    _req: &Request,
    _ino: INodeNo,
    fh: FileHandle,
    _lock_owner: LockOwner,
    reply: ReplyEmpty,
  ) {
    // The handle's own entry is cloned out: the push below must not hold a
    // lock on the open-file map.
    let handle = match self.open_files.get(&fh.0) {
      Some(e) => (e.ino, Arc::clone(&e.copy)),
      None => {
        reply.error(err(libc::EBADF));
        return;
      }
    };
    let (ino, copy) = handle;

    match self.push_if_dirty(ino, &copy) {
      Ok(()) => reply.ok(),
      Err(e) => reply.error(e),
    }
  }

  fn release(
    &self,
    _req: &Request,
    _ino: INodeNo,
    fh: FileHandle,
    _flags: OpenFlags,
    _lock_owner: Option<LockOwner>,
    _flush: bool,
    reply: ReplyEmpty,
  ) {
    if let Some((_, open_file)) = self.open_files.remove(&fh.0) {
      self.release_copy(open_file.ino, &open_file.copy);
    }
    reply.ok();
  }

  fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
    reply.ok();
  }

  fn mkdir(
    &self,
    _req: &Request,
    parent: INodeNo,
    name: &OsStr,
    _mode: u32,
    _umask: u32,
    reply: ReplyEntry,
  ) {
    let parent_path = match self.get_path(parent.0) {
      Some(p) => p,
      None => {
        reply.error(err(libc::ENOENT));
        return;
      }
    };
    let full_path = Self::child_path(&parent_path, name);
    debug!(path = %full_path, "mkdir");

    if let Err(e) = self.ops.mkdir(&full_path) {
      reply.error(device_err(e));
      return;
    }
    self.cache.invalidate(&full_path);

    match self.ops.get_metadata(&full_path) {
      Ok(meta) => {
        let ino = self.get_or_assign_ino(&full_path);
        let attr = self.meta_to_attr(ino, &meta);
        self.cache.insert(full_path, Some(meta));
        reply.entry(&self.opts.entry_timeout, &attr, Generation(0));
      }
      Err(e) => reply.error(device_err(e)),
    }
  }

  fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
    let parent_path = match self.get_path(parent.0) {
      Some(p) => p,
      None => {
        reply.error(err(libc::ENOENT));
        return;
      }
    };
    let full_path = Self::child_path(&parent_path, name);
    debug!(path = %full_path, "unlink");

    if let Err(e) = self.ops.rm(&full_path) {
      reply.error(device_err(e));
      return;
    }
    self.cache.invalidate(&full_path);
    reply.ok();
  }

  fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
    let parent_path = match self.get_path(parent.0) {
      Some(p) => p,
      None => {
        reply.error(err(libc::ENOENT));
        return;
      }
    };
    let full_path = Self::child_path(&parent_path, name);
    debug!(path = %full_path, "rmdir");

    if let Err(e) = self.ops.rmdir(&full_path) {
      reply.error(device_err(e));
      return;
    }
    self.cache.invalidate_prefix(&full_path);
    reply.ok();
  }

  fn rename(
    &self,
    _req: &Request,
    parent: INodeNo,
    name: &OsStr,
    newparent: INodeNo,
    newname: &OsStr,
    _flags: RenameFlags,
    reply: ReplyEmpty,
  ) {
    let from = self.get_path(parent.0).map(|p| Self::child_path(&p, name));
    let to = self
      .get_path(newparent.0)
      .map(|p| Self::child_path(&p, newname));
    let (from, to) = match (from, to) {
      (Some(f), Some(t)) => (f, t),
      _ => {
        reply.error(err(libc::ENOENT));
        return;
      }
    };
    debug!(from = %from, to = %to, "rename");

    if let Err(e) = self.ops.mv(&from, &to) {
      reply.error(device_err(e));
      return;
    }
    self.rekey_paths(&from, &to);
    // Prefix, not exact: renaming a directory moves every descendant with it.
    self.cache.invalidate_prefix(&from);
    self.cache.invalidate_prefix(&to);
    reply.ok();
  }

  fn setattr(
    &self,
    _req: &Request,
    ino: INodeNo,
    _mode: Option<u32>,
    _uid: Option<u32>,
    _gid: Option<u32>,
    size: Option<u64>,
    atime: Option<TimeOrNow>,
    mtime: Option<TimeOrNow>,
    _ctime: Option<SystemTime>,
    _fh: Option<FileHandle>,
    _crtime: Option<SystemTime>,
    _chgtime: Option<SystemTime>,
    _bkuptime: Option<SystemTime>,
    _flags: Option<fuser::BsdFileFlags>,
    reply: ReplyAttr,
  ) {
    let ino_raw = ino.0;
    let path = match self.get_path(ino_raw) {
      Some(p) => p,
      None => {
        reply.error(err(libc::ENOENT));
        return;
      }
    };
    trace!(path = %path, "setattr");

    // Handle truncate
    if let Some(new_size) = size {
      // Truncating to zero replaces the whole file, so the pull is pointless.
      let init = if new_size == 0 {
        CopyInit::Empty
      } else {
        CopyInit::Pull
      };
      let copy = match self.acquire_copy(ino_raw, &path, init) {
        Ok(c) => c,
        Err(e) => {
          reply.error(e);
          return;
        }
      };
      let truncated = std::fs::OpenOptions::new()
        .write(true)
        .open(&copy.path)
        .and_then(|f| f.set_len(new_size))
        .map_err(io_err)
        .and_then(|()| {
          copy.dirty.store(true, Ordering::Release);
          self.push_if_dirty(ino_raw, &copy)
        });
      self.release_copy(ino_raw, &copy);
      if let Err(e) = truncated {
        reply.error(e);
        return;
      }
    }

    // Handle utimens
    let resolve_time = |t: TimeOrNow| -> SystemTime {
      match t {
        TimeOrNow::SpecificTime(st) => st,
        TimeOrNow::Now => SystemTime::now(),
      }
    };
    let at = atime.map(resolve_time);
    let mt = mtime.map(resolve_time);
    if at.is_some() || mt.is_some() {
      if let Err(e) = self.ops.touch(&path, at, mt) {
        reply.error(device_err(e));
        return;
      }
      self.cache.invalidate(&path);
    }

    match self.ops.get_metadata(&path) {
      Ok(meta) => {
        let attr = self.meta_to_attr(ino_raw, &meta);
        self.cache.insert(path, Some(meta));
        reply.attr(&self.opts.attr_timeout, &attr);
      }
      Err(e) => reply.error(device_err(e)),
    }
  }

  fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
    let ino_raw = ino.0;
    let path = match self.get_path(ino_raw) {
      Some(p) => p,
      None => {
        reply.error(err(libc::ENOENT));
        return;
      }
    };
    trace!(path = %path, "readlink");

    let meta = match self.fetch_meta(&path) {
      Ok(m) => m,
      Err(e) => {
        reply.error(device_err(e));
        return;
      }
    };

    match self.ops.resolve_symlink(&path, &meta.raw_line) {
      Ok(target) => reply.data(target.as_bytes()),
      Err(e) => reply.error(device_err(e)),
    }
  }

  fn create(
    &self,
    _req: &Request,
    parent: INodeNo,
    name: &OsStr,
    _mode: u32,
    _umask: u32,
    _flags: i32,
    reply: ReplyCreate,
  ) {
    let parent_path = match self.get_path(parent.0) {
      Some(p) => p,
      None => {
        reply.error(err(libc::ENOENT));
        return;
      }
    };
    let full_path = Self::child_path(&parent_path, name);
    debug!(path = %full_path, "create");

    let ino = self.get_or_assign_ino(&full_path);
    let copy = match self.acquire_copy(ino, &full_path, CopyInit::Empty) {
      Ok(c) => c,
      Err(e) => {
        reply.error(e);
        return;
      }
    };

    let opened = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .open(&copy.path)
      .map_err(io_err)
      // Create the file on the device now, so that a lookup between here and
      // the first flush finds it.
      .and_then(|file| self.push_if_dirty(ino, &copy).map(|()| file))
      .and_then(|file| {
        self
          .ops
          .get_metadata(&full_path)
          .map(|meta| (file, meta))
          .map_err(device_err)
      });

    let (file, meta) = match opened {
      Ok(v) => v,
      Err(e) => {
        self.release_copy(ino, &copy);
        reply.error(e);
        return;
      }
    };

    let attr = self.meta_to_attr(ino, &meta);
    self.cache.insert(full_path, Some(meta));

    let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
    self.open_files.insert(fh, OpenFile { ino, copy, file });
    reply.created(
      &self.opts.entry_timeout,
      &attr,
      Generation(0),
      FileHandle(fh),
      self.file_open_flags(),
    );
  }

  fn mknod(
    &self,
    _req: &Request,
    parent: INodeNo,
    name: &OsStr,
    mode: u32,
    _umask: u32,
    rdev: u32,
    reply: ReplyEntry,
  ) {
    let parent_path = match self.get_path(parent.0) {
      Some(p) => p,
      None => {
        reply.error(err(libc::ENOENT));
        return;
      }
    };
    let full_path = Self::child_path(&parent_path, name);
    debug!(path = %full_path, "mknod");

    // Its own directory: `mknod(2)` needs a path that does not exist yet, and
    // the node is scratch — only the push matters.
    let scratch = match tempfile::TempDir::new_in(self.tmp_dir.path()) {
      Ok(d) => d,
      Err(e) => {
        reply.error(io_err(e));
        return;
      }
    };
    let local = scratch.path().join("node");

    let c_path = match std::ffi::CString::new(local.to_string_lossy().as_ref()) {
      Ok(p) => p,
      Err(_) => {
        reply.error(err(libc::EINVAL));
        return;
      }
    };
    let ret = unsafe { libc::mknod(c_path.as_ptr(), mode, rdev as libc::dev_t) };
    if ret != 0 {
      reply.error(io_err(std::io::Error::last_os_error()));
      return;
    }

    if let Err(e) = self.push_and_sync(&local, &full_path) {
      reply.error(device_err(e));
      return;
    }

    self.cache.invalidate(&full_path);
    drop(scratch);

    match self.ops.get_metadata(&full_path) {
      Ok(meta) => {
        let ino = self.get_or_assign_ino(&full_path);
        let attr = self.meta_to_attr(ino, &meta);
        self.cache.insert(full_path, Some(meta));
        reply.entry(&self.opts.entry_timeout, &attr, Generation(0));
      }
      Err(e) => reply.error(device_err(e)),
    }
  }
}

/// Mount `adbfs` and serve requests until the filesystem is unmounted.
///
/// `on_mounted` runs after the mount is live and the kernel handshake is done,
/// but before any worker thread exists. That is the only safe point to fork
/// into the background: earlier and the parent would report success before the
/// mountpoint works, later and `fork` would drop the worker threads.
pub fn mount(
  adbfs: AdbFs,
  mountpoint: &std::path::Path,
  options: Vec<String>,
  single_threaded: bool,
  on_mounted: impl FnOnce() -> color_eyre::Result<()>,
) -> color_eyre::Result<()> {
  let mut mount_options = vec![
    MountOption::AutoUnmount,
    MountOption::FSName("adbfs".to_string()),
  ];
  for opt in options {
    mount_options.push(MountOption::CUSTOM(opt));
  }

  let n_threads = if single_threaded {
    1
  } else {
    std::thread::available_parallelism()
      .map(|n| n.get())
      .unwrap_or(4)
  };

  let mut config = Config::default();
  config.mount_options = mount_options;
  config.acl = SessionACL::RootAndOwner;
  config.n_threads = Some(n_threads);
  config.clone_fd = true;

  let session = Session::new(adbfs, mountpoint, &config)?;
  on_mounted()?;
  session.spawn()?.join()?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::adb::mock::MockAdb;
  use crate::ops::ResolvedCompat;

  fn test_fs(adb: Arc<MockAdb>) -> AdbFs {
    let ops = Arc::new(DeviceOps::new(adb, ResolvedCompat::legacy()));
    AdbFs::new(ops, Duration::from_secs(30), FsOptions::default()).unwrap()
  }

  /// Two opens of one file must share one local copy, so that a write through
  /// one handle is visible to the other and neither pushes a stale copy.
  #[test]
  fn concurrent_opens_share_one_local_copy() {
    let adb = Arc::new(MockAdb::silent().with_content(b"device contents"));
    let fs = test_fs(adb);
    let ino = fs.get_or_assign_ino("/sdcard/a.txt");

    let first = fs
      .acquire_copy(ino, "/sdcard/a.txt", CopyInit::Pull)
      .unwrap();
    let second = fs
      .acquire_copy(ino, "/sdcard/a.txt", CopyInit::Pull)
      .unwrap();

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(std::fs::read(&first.path).unwrap(), b"device contents");

    // The copy outlives the first release; only the last one deletes it.
    fs.release_copy(ino, &first);
    assert!(first.path.exists());
    fs.release_copy(ino, &second);
    assert!(!second.path.exists());
    assert!(fs.open_copies.is_empty());
  }

  /// The local name comes from the inode, so paths that differ only in a
  /// separator cannot collide, and no path is too long to open.
  #[test]
  fn local_paths_do_not_collide() {
    let fs = test_fs(Arc::new(MockAdb::silent()));
    let deep = format!("/sdcard/{}", "x/".repeat(200));

    let paths: Vec<PathBuf> = ["/a/b", "/a_b", &deep]
      .iter()
      .map(|p| fs.local_path_for(fs.get_or_assign_ino(p)))
      .collect();

    assert_eq!(
      paths.iter().collect::<std::collections::HashSet<_>>().len(),
      3
    );
    for path in &paths {
      assert!(path.file_name().unwrap().len() <= 255);
    }
  }

  /// A failed pull must not leave an unpopulated copy in the map for the next
  /// open to find and treat as the file's contents.
  #[test]
  fn failed_pull_leaves_no_copy_behind() {
    let fs = test_fs(Arc::new(MockAdb::silent().with_failing_pull()));
    let ino = fs.get_or_assign_ino("/sdcard/a.txt");

    assert!(
      fs.acquire_copy(ino, "/sdcard/a.txt", CopyInit::Pull)
        .is_err()
    );
    assert!(fs.open_copies.is_empty());
    assert!(!fs.local_path_for(ino).exists());
  }

  #[test]
  fn rename_remaps_the_inode() {
    let fs = test_fs(Arc::new(MockAdb::silent()));
    let ino = fs.get_or_assign_ino("/sdcard/old.txt");

    fs.rekey_paths("/sdcard/old.txt", "/sdcard/new.txt");

    assert_eq!(fs.get_path(ino).as_deref(), Some("/sdcard/new.txt"));
    assert_eq!(fs.get_or_assign_ino("/sdcard/new.txt"), ino);
    assert_ne!(fs.get_or_assign_ino("/sdcard/old.txt"), ino);
  }

  /// A handle open across a rename pushes to the new path: the device path is
  /// resolved from the inode at flush time, not captured at open time.
  #[test]
  fn dirty_copy_pushes_to_the_renamed_path() {
    let adb = Arc::new(MockAdb::silent().with_content(b"x"));
    let fs = test_fs(Arc::clone(&adb));
    let ino = fs.get_or_assign_ino("/sdcard/old.txt");
    let copy = fs
      .acquire_copy(ino, "/sdcard/old.txt", CopyInit::Pull)
      .unwrap();
    copy.dirty.store(true, Ordering::Release);

    fs.rekey_paths("/sdcard/old.txt", "/sdcard/new.txt");
    fs.push_if_dirty(ino, &copy).unwrap();

    let (local, remote) = adb.pushes().pop().expect("a push");
    assert_eq!(remote, "/sdcard/new.txt");
    assert_eq!(local, copy.path);
    assert!(!copy.dirty.load(Ordering::Acquire));
    fs.release_copy(ino, &copy);
  }

  #[test]
  fn rename_remaps_descendants_but_not_siblings() {
    let fs = test_fs(Arc::new(MockAdb::silent()));
    let child = fs.get_or_assign_ino("/sdcard/dir/deep/file.txt");
    let sibling = fs.get_or_assign_ino("/sdcard/dirtyfile");

    fs.rekey_paths("/sdcard/dir", "/sdcard/moved");

    assert_eq!(
      fs.get_path(child).as_deref(),
      Some("/sdcard/moved/deep/file.txt")
    );
    assert_eq!(fs.get_path(sibling).as_deref(), Some("/sdcard/dirtyfile"));
  }

  /// Renaming onto an existing file makes the overwritten inode unreachable.
  #[test]
  fn rename_over_existing_drops_the_replaced_inode() {
    let fs = test_fs(Arc::new(MockAdb::silent()));
    let source = fs.get_or_assign_ino("/sdcard/a.txt");
    let replaced = fs.get_or_assign_ino("/sdcard/b.txt");

    fs.rekey_paths("/sdcard/a.txt", "/sdcard/b.txt");

    assert_eq!(fs.get_path(source).as_deref(), Some("/sdcard/b.txt"));
    assert_eq!(fs.get_path(replaced), None);
  }
}
