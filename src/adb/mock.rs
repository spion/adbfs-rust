//! Test double for [`AdbDevice`].
//!
//! Shell responses are served from a queue in call order. An exhausted queue
//! answers with silence and exit 0, which is what a successful mutation looks
//! like.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::{AdbDevice, AdbError, ShellOutput};

/// A command that printed `lines` on stdout and exited 0. Also stands for the
/// pre-shell-protocol-v2 failure: those devices fold stderr into stdout and
/// still exit 0.
pub fn out(lines: &[&str]) -> Result<ShellOutput, AdbError> {
  Ok(ShellOutput {
    stdout: lines.iter().map(|s| (*s).to_owned()).collect(),
    stderr: Vec::new(),
    exit_code: 0,
  })
}

/// A command that failed the way shell protocol v2 reports it: message on
/// stderr, non-zero exit.
pub fn failed(exit_code: i32, stderr: &[&str]) -> Result<ShellOutput, AdbError> {
  Ok(ShellOutput {
    stdout: Vec::new(),
    stderr: stderr.iter().map(|s| (*s).to_owned()).collect(),
    exit_code,
  })
}

pub struct MockAdb {
  responses: Mutex<VecDeque<Result<ShellOutput, AdbError>>>,
  commands: Mutex<Vec<String>>,
  pushes: Mutex<Vec<(PathBuf, String)>>,
  pull_content: Vec<u8>,
  pull_fails: bool,
}

impl MockAdb {
  pub fn new(responses: Vec<Result<ShellOutput, AdbError>>) -> Self {
    Self {
      responses: Mutex::new(responses.into()),
      commands: Mutex::new(Vec::new()),
      pushes: Mutex::new(Vec::new()),
      pull_content: Vec::new(),
      pull_fails: false,
    }
  }

  /// A device that answers every command with silence.
  pub fn silent() -> Self {
    Self::new(Vec::new())
  }

  /// What `pull` writes to the local path.
  pub fn with_content(mut self, content: &[u8]) -> Self {
    self.pull_content = content.to_vec();
    self
  }

  /// A device whose files cannot be read.
  pub fn with_failing_pull(mut self) -> Self {
    self.pull_fails = true;
    self
  }

  pub fn commands(&self) -> Vec<String> {
    self.commands.lock().unwrap().clone()
  }

  pub fn pushes(&self) -> Vec<(PathBuf, String)> {
    self.pushes.lock().unwrap().clone()
  }

  fn next(&self, command: &str) -> Result<ShellOutput, AdbError> {
    self.commands.lock().unwrap().push(command.to_owned());
    self
      .responses
      .lock()
      .unwrap()
      .pop_front()
      .unwrap_or_else(|| out(&[]))
  }
}

impl AdbDevice for MockAdb {
  fn shell(&self, command: &str) -> Result<Vec<String>, AdbError> {
    self.next(command).map(|o| o.stdout)
  }

  fn shell_with_stderr(&self, command: &str) -> Result<ShellOutput, AdbError> {
    self.next(command)
  }

  fn pull(&self, _remote: &Path, local: &Path) -> Result<(), AdbError> {
    if self.pull_fails {
      return Err(AdbError::CommandFailed {
        exit_code: 1,
        stderr: "remote object does not exist".to_owned(),
      });
    }
    std::fs::write(local, &self.pull_content)?;
    Ok(())
  }

  fn push(&self, local: &Path, remote: &Path) -> Result<(), AdbError> {
    self
      .pushes
      .lock()
      .unwrap()
      .push((local.to_owned(), remote.to_string_lossy().into_owned()));
    Ok(())
  }

  fn sync_device(&self) -> Result<(), AdbError> {
    Ok(())
  }
}
