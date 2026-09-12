use std::path::Path;
use thiserror::Error;

#[derive(Debug)]
pub struct ShellOutput {
  pub stdout: Vec<String>,
  #[allow(dead_code)]
  pub stderr: Vec<String>,
  #[allow(dead_code)]
  pub exit_code: i32,
}

#[derive(Debug, Error)]
pub enum AdbError {
  #[error("device not found")]
  DeviceNotFound,
  #[error("permission denied: {path}")]
  #[allow(dead_code)]
  PermissionDenied { path: String },
  #[error("command failed (exit {exit_code}): {stderr}")]
  CommandFailed { exit_code: i32, stderr: String },
  #[error(transparent)]
  Io(#[from] std::io::Error),
}

/// Transport to a device. Blocking: every call runs on the FUSE worker thread
/// that issued it, and concurrency comes from the FUSE session's thread pool.
pub trait AdbDevice: Send + Sync {
  fn shell(&self, command: &str) -> Result<Vec<String>, AdbError>;
  fn shell_with_stderr(&self, command: &str) -> Result<ShellOutput, AdbError>;
  fn pull(&self, remote: &Path, local: &Path) -> Result<(), AdbError>;
  fn push(&self, local: &Path, remote: &Path) -> Result<(), AdbError>;
  fn sync_device(&self) -> Result<(), AdbError>;
}

pub mod cli;
#[cfg(test)]
pub mod mock;
