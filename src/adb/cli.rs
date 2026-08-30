use super::{AdbDevice, AdbError, ShellOutput};
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output};
use tracing::debug;

pub struct AdbCli {
  serial: Option<String>,
}

impl AdbCli {
  pub fn new(serial: Option<String>) -> Self {
    Self { serial }
  }

  fn run<I, S>(&self, args: I) -> Result<Output, AdbError>
  where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
  {
    let mut cmd = Command::new("adb");
    if let Some(ref serial) = self.serial {
      cmd.arg("-s").arg(serial);
    }
    cmd.args(args).output().map_err(Into::into)
  }

  fn transfer(&self, verb: &str, from: &Path, to: &Path) -> Result<(), AdbError> {
    let output = self.run([OsStr::new(verb), from.as_os_str(), to.as_os_str()])?;
    if !output.status.success() {
      return Err(AdbError::CommandFailed {
        exit_code: output.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
      });
    }
    Ok(())
  }
}

fn to_lines(bytes: &[u8]) -> Vec<String> {
  String::from_utf8_lossy(bytes)
    .lines()
    .map(|l| l.trim_end_matches('\r').to_string())
    .collect()
}

impl AdbDevice for AdbCli {
  fn shell(&self, command: &str) -> Result<Vec<String>, AdbError> {
    debug!(cmd = command, "adb shell");
    let output = self.run(["shell", command])?;

    let lines = to_lines(&output.stdout);
    if lines.is_empty() && !output.status.success() {
      return Err(AdbError::DeviceNotFound);
    }
    Ok(lines)
  }

  fn shell_with_stderr(&self, command: &str) -> Result<ShellOutput, AdbError> {
    debug!(cmd = command, "adb shell (with stderr)");
    let output = self.run(["shell", command])?;

    Ok(ShellOutput {
      stdout: to_lines(&output.stdout),
      stderr: to_lines(&output.stderr),
      exit_code: output.status.code().unwrap_or(-1),
    })
  }

  fn pull(&self, remote: &Path, local: &Path) -> Result<(), AdbError> {
    debug!(?remote, ?local, "adb pull");
    self.transfer("pull", remote, local)
  }

  fn push(&self, local: &Path, remote: &Path) -> Result<(), AdbError> {
    debug!(?local, ?remote, "adb push");
    self.transfer("push", local, remote)
  }

  fn sync_device(&self) -> Result<(), AdbError> {
    self.shell("sync")?;
    Ok(())
  }
}
