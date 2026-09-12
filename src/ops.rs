use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;
use thiserror::Error;
use tracing::debug;

use crate::adb::{AdbDevice, AdbError, ShellOutput};
use crate::escape::shell_escape_path;
use crate::parse::{self, FileMeta};

const PERMISSION_ERR_SUFFIX: &str = ": Permission denied";
const TOUCH_TOYBOX_ERR_PREFIX: &str = "touch: bad '@";
const TOUCH_BUSYBOX_ERR_PREFIX: &str = "touch: invalid date '@";

#[derive(Debug, Error)]
pub enum DeviceError {
  #[error("no device connected")]
  NoDevice,
  #[error("permission denied: {path}")]
  PermissionDenied { path: String },
  #[error("file not found: {path}")]
  NotFound { path: String },
  #[error("not supported")]
  NotSupported,
  /// A command on the device reported a failure. `errno` is the translation of
  /// the device's own error message, so callers see `ENOTEMPTY`/`EEXIST`/… and
  /// not a blanket `EIO`.
  #[error("{message}")]
  Failed { errno: i32, message: String },
  #[error(transparent)]
  Adb(#[from] AdbError),
}

impl DeviceError {
  pub fn to_errno(&self) -> i32 {
    match self {
      Self::NoDevice => libc::EAGAIN,
      Self::PermissionDenied { .. } => libc::EACCES,
      Self::NotFound { .. } => libc::ENOENT,
      Self::NotSupported => libc::ENOSYS,
      Self::Failed { errno, .. } => *errno,
      Self::Adb(_) => libc::EIO,
    }
  }
}

pub struct ResolvedCompat {
  // Future: metadata_strategy, symlink_strategy, etc.
}

impl ResolvedCompat {
  pub fn legacy() -> Self {
    Self {}
  }
}

pub struct DeviceOps {
  adb: Arc<dyn AdbDevice>,
  #[allow(dead_code)]
  compat: ResolvedCompat,
  touch_gnu_mode: AtomicBool,
  pub rescan: bool,
}

impl DeviceOps {
  pub fn new(adb: Arc<dyn AdbDevice>, compat: ResolvedCompat) -> Self {
    Self {
      adb,
      compat,
      touch_gnu_mode: AtomicBool::new(true),
      rescan: false,
    }
  }

  pub fn with_rescan(mut self, rescan: bool) -> Self {
    self.rescan = rescan;
    self
  }

  pub fn get_metadata(&self, path: &str) -> Result<FileMeta, DeviceError> {
    let escaped = shell_escape_path(path);
    let cmd = format!("ls -l -a -d '{escaped}'");
    let output = self.adb.shell_with_stderr(&cmd)?;

    // On modern Android, adb shell separates stdout/stderr.
    // When ls fails, the error message is in stderr, not stdout.
    let lines = if output.stdout.is_empty() && !output.stderr.is_empty() {
      &output.stderr
    } else {
      &output.stdout
    };

    if lines.is_empty() {
      return Err(DeviceError::NoDevice);
    }

    let first = &lines[0];
    if first.ends_with(PERMISSION_ERR_SUFFIX) {
      return Err(DeviceError::PermissionDenied {
        path: path.to_string(),
      });
    }

    parse::parse_ls_line(first).ok_or_else(|| DeviceError::NotFound {
      path: path.to_string(),
    })
  }

  pub fn list_dir(&self, path: &str) -> Result<Vec<(String, Option<FileMeta>)>, DeviceError> {
    let escaped = shell_escape_path(path);
    let cmd = format!("ls -l -a '{escaped}'");
    let lines = self.adb.shell(&cmd)?;

    let mut entries = Vec::new();
    for line in &lines {
      if line.len() < 3 {
        continue;
      }
      if !parse::is_valid_ls_output(line) {
        if line.ends_with(PERMISSION_ERR_SUFFIX)
          && let Some(name) = extract_name_from_perm_error(line)
        {
          entries.push((name, None));
        }
        continue;
      }
      if let Some(name) = parse::extract_filename(line) {
        let meta = parse::parse_ls_line(line);
        entries.push((name.to_string(), meta));
      }
    }
    Ok(entries)
  }

  pub fn resolve_symlink(&self, path: &str, raw_line: &str) -> Result<String, DeviceError> {
    let num_slashes = path.chars().filter(|&c| c == '/').count();
    parse::parse_symlink_target(raw_line, num_slashes).ok_or(DeviceError::NotSupported)
  }

  pub fn touch(
    &self,
    path: &str,
    atime: Option<SystemTime>,
    mtime: Option<SystemTime>,
  ) -> Result<(), DeviceError> {
    let escaped = shell_escape_path(path);
    let gnu = self.touch_gnu_mode.load(Ordering::Relaxed);
    let mut parts = Vec::new();

    if let Some(at) = atime {
      parts.push(format!(
        "touch -a -d {} '{escaped}'",
        format_touch_time(at, gnu)
      ));
    }
    if let Some(mt) = mtime {
      parts.push(format!(
        "touch -m -d {} '{escaped}'",
        format_touch_time(mt, gnu)
      ));
    }
    if parts.is_empty() {
      return Ok(());
    }

    let cmd = parts.join(" && ");
    let output = self.adb.shell_with_stderr(&cmd)?;

    if let Some(message) = first_message(&output) {
      let is_date_err = message.starts_with(TOUCH_TOYBOX_ERR_PREFIX)
        || message.starts_with(TOUCH_BUSYBOX_ERR_PREFIX);
      if is_date_err {
        if !gnu {
          return Err(DeviceError::NotSupported);
        }
        debug!("Touch doesn't support GNU dates, switching to legacy mode");
        self.touch_gnu_mode.store(false, Ordering::Relaxed);
        return self.touch(path, atime, mtime);
      }
      return Err(DeviceError::Failed {
        errno: errno_for_message(message),
        message: message.to_owned(),
      });
    }

    if self.rescan {
      self.rescan_file(path)?;
    }
    Ok(())
  }

  pub fn mkdir(&self, path: &str) -> Result<(), DeviceError> {
    let escaped = shell_escape_path(path);
    self.run_mutation(&format!("mkdir '{escaped}'"))
  }

  pub fn rm(&self, path: &str) -> Result<(), DeviceError> {
    let escaped = shell_escape_path(path);
    self.run_mutation(&format!("rm '{escaped}'"))?;
    if self.rescan {
      self.rescan_file(path)?;
    }
    Ok(())
  }

  pub fn rmdir(&self, path: &str) -> Result<(), DeviceError> {
    let escaped = shell_escape_path(path);
    self.run_mutation(&format!("rmdir '{escaped}'"))?;
    if self.rescan {
      self.rescan_dir_removed(path)?;
    }
    Ok(())
  }

  pub fn mv(&self, from: &str, to: &str) -> Result<(), DeviceError> {
    let from_escaped = shell_escape_path(from);
    let to_escaped = shell_escape_path(to);
    self.run_mutation(&format!("mv '{from_escaped}' '{to_escaped}'"))?;
    if self.rescan {
      self.rescan_file(from)?;
      self.rescan_file(to)?;
    }
    Ok(())
  }

  /// Runs a command that is silent on success (`mkdir`, `rm`, `rmdir`, `mv`).
  ///
  /// Any line the command prints is therefore a failure, which is the only
  /// signal available on devices without shell protocol v2: there `adb shell`
  /// always exits 0, so the exit code alone would report every failure as
  /// success.
  fn run_mutation(&self, cmd: &str) -> Result<(), DeviceError> {
    let output = self.adb.shell_with_stderr(cmd)?;
    match first_message(&output) {
      Some(message) => Err(DeviceError::Failed {
        errno: errno_for_message(message),
        message: message.to_owned(),
      }),
      None if output.exit_code != 0 => Err(DeviceError::Failed {
        errno: libc::EIO,
        message: format!("`{cmd}` exited with {}", output.exit_code),
      }),
      None => Ok(()),
    }
  }

  pub fn pull(&self, remote: &str, local: &Path) -> Result<(), DeviceError> {
    self.adb.pull(Path::new(remote), local).map_err(Into::into)
  }

  pub fn push(&self, local: &Path, remote: &str) -> Result<(), DeviceError> {
    self.adb.push(local, Path::new(remote)).map_err(Into::into)
  }

  pub fn sync_device(&self) -> Result<(), DeviceError> {
    self.adb.sync_device().map_err(Into::into)
  }

  // `am broadcast` prints the intent it sent on success, so these keep using
  // `shell` rather than `run_mutation`.
  fn rescan_file(&self, path: &str) -> Result<(), DeviceError> {
    let escaped = shell_escape_path(path);
    let cmd = format!(
      "am broadcast -a android.intent.action.MEDIA_SCANNER_SCAN_FILE -d 'file://{escaped}'"
    );
    self.adb.shell(&cmd)?;
    Ok(())
  }

  fn rescan_dir_removed(&self, path: &str) -> Result<(), DeviceError> {
    let escaped = shell_escape_path(path);
    let cmd =
      format!("am broadcast -a android.intent.action.MEDIA_UNMOUNTED -d 'file://{escaped}'");
    self.adb.shell(&cmd)?;
    Ok(())
  }
}

fn format_touch_time(time: SystemTime, gnu_format: bool) -> String {
  let dur = time
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default();
  if gnu_format {
    format!("@{}.{:09}", dur.as_secs(), dur.subsec_nanos())
  } else {
    format!(
      "`date -ud @{}.{:09} +%Y-%m-%dT%H:%M:%S`",
      dur.as_secs(),
      dur.subsec_nanos()
    )
  }
}

/// The first non-blank line the device wrote, on either stream. Devices
/// without shell protocol v2 fold stderr into stdout, so both are checked.
fn first_message(output: &ShellOutput) -> Option<&str> {
  output
    .stderr
    .iter()
    .chain(output.stdout.iter())
    .map(|l| l.trim())
    .find(|l| !l.is_empty())
}

/// Translates a device error message to an errno.
///
/// toybox, toolbox and busybox word the prefix differently but all end the
/// message with the `strerror` text, so the match is on that tail.
fn errno_for_message(message: &str) -> i32 {
  const TABLE: &[(&str, i32)] = &[
    ("permission denied", libc::EACCES),
    ("operation not permitted", libc::EPERM),
    ("no such file or directory", libc::ENOENT),
    ("directory not empty", libc::ENOTEMPTY),
    ("file exists", libc::EEXIST),
    ("read-only file system", libc::EROFS),
    ("is a directory", libc::EISDIR),
    ("not a directory", libc::ENOTDIR),
    ("cross-device link", libc::EXDEV),
    ("no space left on device", libc::ENOSPC),
    ("device or resource busy", libc::EBUSY),
    ("file name too long", libc::ENAMETOOLONG),
    ("invalid argument", libc::EINVAL),
  ];
  let lower = message.to_ascii_lowercase();
  TABLE
    .iter()
    .find(|(needle, _)| lower.contains(needle))
    .map_or(libc::EIO, |(_, errno)| *errno)
}

fn extract_name_from_perm_error(line: &str) -> Option<String> {
  // Format: "lstat '//efs' failed: Permission denied"
  let last_slash = line.rfind('/')?;
  let end = line.find("' ")?;
  if last_slash < end {
    Some(line[last_slash + 1..end].to_string())
  } else {
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::adb::mock::{MockAdb, failed, out};

  #[test]
  fn get_metadata_parses_ls_output() {
    let mock = Arc::new(MockAdb::new(vec![out(&[
      "-rw-r--r-- root root 1234 2024-01-15 10:30 test.txt",
    ])]));
    let ops = DeviceOps::new(mock, ResolvedCompat::legacy());
    let meta = ops.get_metadata("/sdcard/test.txt").unwrap();
    assert_eq!(meta.size, 1234);
    assert_eq!(meta.mode & libc::S_IFREG as u32, libc::S_IFREG as u32);
  }

  #[test]
  fn get_metadata_permission_denied() {
    let mock = Arc::new(MockAdb::new(vec![out(&[
      "/sbin/healthd: Permission denied",
    ])]));
    let ops = DeviceOps::new(mock, ResolvedCompat::legacy());
    let result = ops.get_metadata("/sbin/healthd");
    assert!(matches!(result, Err(DeviceError::PermissionDenied { .. })));
  }

  #[test]
  fn get_metadata_no_device() {
    let mock = Arc::new(MockAdb::new(vec![out(&[])]));
    let ops = DeviceOps::new(mock, ResolvedCompat::legacy());
    let result = ops.get_metadata("/any");
    assert!(matches!(result, Err(DeviceError::NoDevice)));
  }

  #[test]
  fn get_metadata_not_found() {
    let mock = Arc::new(MockAdb::new(vec![out(&[
      "/sdcard/nofile: No such file or directory",
    ])]));
    let ops = DeviceOps::new(mock, ResolvedCompat::legacy());
    let result = ops.get_metadata("/sdcard/nofile");
    assert!(matches!(result, Err(DeviceError::NotFound { .. })));
  }

  #[test]
  fn list_dir_parses_entries() {
    let mock = Arc::new(MockAdb::new(vec![out(&[
      "-rw-r--r-- root root 100 2024-01-15 10:30 a.txt",
      "drwxr-xr-x root root      2024-01-15 10:30 subdir",
    ])]));
    let ops = DeviceOps::new(mock, ResolvedCompat::legacy());
    let entries = ops.list_dir("/sdcard").unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, "a.txt");
    assert_eq!(entries[1].0, "subdir");
    assert!(entries[0].1.is_some());
    assert!(entries[1].1.is_some());
  }

  #[test]
  fn list_dir_handles_permission_errors() {
    let mock = Arc::new(MockAdb::new(vec![out(&[
      "-rw-r--r-- root root 100 2024-01-15 10:30 ok.txt",
      "lstat '//efs' failed: Permission denied",
    ])]));
    let ops = DeviceOps::new(mock, ResolvedCompat::legacy());
    let entries = ops.list_dir("/").unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, "ok.txt");
    assert_eq!(entries[1].0, "efs");
    assert!(entries[1].1.is_none());
  }

  #[test]
  fn touch_gnu_fallback() {
    let mock = Arc::new(MockAdb::new(vec![
      // First attempt: GNU format fails
      out(&["touch: bad '@1700000000.000000000'"]),
      // Second attempt: legacy format succeeds
      out(&[]),
    ]));
    let ops = DeviceOps::new(mock, ResolvedCompat::legacy());
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1700000000);
    ops.touch("/sdcard/test", None, Some(time)).unwrap();
    assert!(!ops.touch_gnu_mode.load(Ordering::Relaxed));
  }

  fn errno_of(result: Result<(), DeviceError>) -> i32 {
    result.expect_err("should have failed").to_errno()
  }

  #[test]
  fn silent_mutation_succeeds() {
    let ops = DeviceOps::new(Arc::new(MockAdb::silent()), ResolvedCompat::legacy());
    assert!(ops.mkdir("/sdcard/new").is_ok());
    assert!(ops.rm("/sdcard/old").is_ok());
    assert!(ops.mv("/sdcard/a", "/sdcard/b").is_ok());
  }

  #[test]
  fn mutation_maps_device_message_to_errno() {
    let cases: &[(&str, i32)] = &[
      (
        "rmdir: '/sdcard/d' failed: Directory not empty",
        libc::ENOTEMPTY,
      ),
      ("mkdir: '/sdcard/d': File exists", libc::EEXIST),
      ("rm: /system/x: Permission denied", libc::EACCES),
      ("mv: '/a' -> '/b': Cross-device link", libc::EXDEV),
      ("rm: /sdcard/gone: No such file or directory", libc::ENOENT),
      ("mkdir: '/system/x': Read-only file system", libc::EROFS),
      ("rm: /sdcard/d: Is a directory", libc::EISDIR),
      ("something the table does not know", libc::EIO),
    ];
    for (message, errno) in cases {
      let mock = Arc::new(MockAdb::new(vec![failed(1, &[message])]));
      let ops = DeviceOps::new(mock, ResolvedCompat::legacy());
      assert_eq!(errno_of(ops.rmdir("/sdcard/d")), *errno, "for {message}");
    }
  }

  /// Devices without shell protocol v2 exit 0 whatever happened and fold the
  /// message into stdout. The message is the only signal there.
  #[test]
  fn mutation_fails_on_legacy_device_that_exits_zero() {
    let mock = Arc::new(MockAdb::new(vec![out(&[
      "mkdir failed for /sdcard/d, File exists",
    ])]));
    let ops = DeviceOps::new(mock, ResolvedCompat::legacy());
    assert_eq!(errno_of(ops.mkdir("/sdcard/d")), libc::EEXIST);
  }

  #[test]
  fn mutation_fails_on_exit_code_without_message() {
    let mock = Arc::new(MockAdb::new(vec![failed(1, &[])]));
    let ops = DeviceOps::new(mock, ResolvedCompat::legacy());
    assert_eq!(errno_of(ops.rm("/sdcard/x")), libc::EIO);
  }

  #[test]
  fn touch_reports_failure() {
    let mock = Arc::new(MockAdb::new(vec![failed(
      1,
      &["touch: '/system/x': Read-only file system"],
    )]));
    let ops = DeviceOps::new(mock, ResolvedCompat::legacy());
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1700000000);
    assert_eq!(
      errno_of(ops.touch("/system/x", None, Some(time))),
      libc::EROFS
    );
  }

  #[test]
  fn rescan_escapes_the_path() {
    let mock = Arc::new(MockAdb::silent());
    let ops = DeviceOps::new(mock.clone(), ResolvedCompat::legacy()).with_rescan(true);
    ops.rm("/sdcard/'; reboot; '.mp3").unwrap();

    let rescan = mock.commands().pop().expect("rescan command");
    assert!(
      rescan.contains(r"'\''; reboot; '\''"),
      "quote left unescaped: {rescan}"
    );
  }

  #[test]
  fn resolve_symlink_works() {
    let ops = DeviceOps::new(Arc::new(MockAdb::silent()), ResolvedCompat::legacy());
    let raw = "lrwxrwxrwx root root 2024-01-01 12:00 link -> /system/lib/libc.so";
    let target = ops.resolve_symlink("/vendor/lib/link", raw).unwrap();
    assert_eq!(target, "../../system/lib/libc.so");
  }
}
