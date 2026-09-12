use std::time::Duration;

/// libfuse high-level options, which fuser does not implement.
///
/// libfuse's `fuse_main` handled these inside the library; fuser is a
/// low-level binding, so the filesystem has to honor them itself. They must
/// also be consumed before mounting: `fusermount3` rejects every option it
/// does not recognise, so a stray `-o direct_io` fails the whole mount.
#[derive(Debug, Clone, PartialEq)]
pub struct FsOptions {
  pub entry_timeout: Duration,
  pub attr_timeout: Duration,
  pub negative_timeout: Duration,
  pub uid: Option<u32>,
  pub gid: Option<u32>,
  /// Permission bits become `0o777 & !umask`, as libfuse does it.
  pub umask: Option<u32>,
  pub direct_io: bool,
  pub kernel_cache: bool,
  pub debug: bool,
}

impl Default for FsOptions {
  fn default() -> Self {
    Self {
      entry_timeout: Duration::from_secs(1),
      attr_timeout: Duration::from_secs(1),
      negative_timeout: Duration::ZERO,
      uid: None,
      gid: None,
      umask: None,
      direct_io: false,
      kernel_cache: false,
      debug: false,
    }
  }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid mount option -o {key}={value}")]
pub struct OptionError {
  key: String,
  value: String,
}

fn invalid(key: &str, value: &str) -> OptionError {
  OptionError {
    key: key.to_string(),
    value: value.to_string(),
  }
}

fn seconds(key: &str, value: &str) -> Result<Duration, OptionError> {
  let secs: f64 = value.parse().map_err(|_| invalid(key, value))?;
  Duration::try_from_secs_f64(secs).map_err(|_| invalid(key, value))
}

fn number(key: &str, value: &str, radix: u32) -> Result<u32, OptionError> {
  u32::from_str_radix(value, radix).map_err(|_| invalid(key, value))
}

impl FsOptions {
  /// Split `-o` values into filesystem options and leftover mount options.
  ///
  /// Anything not recognised here is passed through untouched, so kernel and
  /// `fusermount3` options keep working.
  pub fn split(options: Vec<String>) -> Result<(Self, Vec<String>), OptionError> {
    let mut fs = Self::default();
    let mut mount = Vec::new();
    for opt in options {
      if !fs.consume(&opt)? {
        mount.push(opt);
      }
    }
    Ok((fs, mount))
  }

  /// Returns whether `opt` was recognised and applied.
  fn consume(&mut self, opt: &str) -> Result<bool, OptionError> {
    let (key, value) = match opt.split_once('=') {
      Some((k, v)) => (k, Some(v)),
      None => (opt, None),
    };
    match (key, value) {
      ("entry_timeout", Some(v)) => self.entry_timeout = seconds(key, v)?,
      ("attr_timeout", Some(v)) => self.attr_timeout = seconds(key, v)?,
      ("negative_timeout", Some(v)) => self.negative_timeout = seconds(key, v)?,
      ("uid", Some(v)) => self.uid = Some(number(key, v, 10)?),
      ("gid", Some(v)) => self.gid = Some(number(key, v, 10)?),
      ("umask", Some(v)) => self.umask = Some(number(key, v, 8)?),
      ("direct_io", None) => self.direct_io = true,
      ("kernel_cache", None) => self.kernel_cache = true,
      ("debug", None) => self.debug = true,
      _ => return Ok(false),
    }
    Ok(true)
  }

  /// Apply `uid`, `gid` and `umask` overrides to device metadata.
  pub fn apply_ownership(&self, mode: u32, uid: &mut u32, gid: &mut u32) -> u32 {
    if let Some(u) = self.uid {
      *uid = u;
    }
    if let Some(g) = self.gid {
      *gid = g;
    }
    match self.umask {
      Some(mask) => (mode & libc::S_IFMT) | (0o777 & !mask),
      None => mode,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn split(args: &[&str]) -> (FsOptions, Vec<String>) {
    FsOptions::split(args.iter().map(|s| s.to_string()).collect()).unwrap()
  }

  #[test]
  fn unknown_options_pass_through() {
    let (fs, mount) = split(&["allow_other", "max_read=65536", "ro"]);
    assert_eq!(fs, FsOptions::default());
    assert_eq!(mount, ["allow_other", "max_read=65536", "ro"]);
  }

  #[test]
  fn known_options_are_consumed() {
    let (fs, mount) = split(&["allow_other", "direct_io", "uid=1000", "attr_timeout=2.5"]);
    assert_eq!(mount, ["allow_other"]);
    assert!(fs.direct_io);
    assert_eq!(fs.uid, Some(1000));
    assert_eq!(fs.attr_timeout, Duration::from_millis(2500));
  }

  #[test]
  fn umask_is_octal() {
    let (fs, _) = split(&["umask=022"]);
    assert_eq!(fs.umask, Some(0o22));
    let mut uid = 0;
    let mut gid = 0;
    let mode = fs.apply_ownership(libc::S_IFREG | 0o600, &mut uid, &mut gid);
    assert_eq!(mode, libc::S_IFREG | 0o755);
  }

  #[test]
  fn ownership_defaults_to_device_values() {
    let (fs, _) = split(&[]);
    let mut uid = 2000;
    let mut gid = 3000;
    let mode = fs.apply_ownership(libc::S_IFREG | 0o600, &mut uid, &mut gid);
    assert_eq!((uid, gid, mode), (2000, 3000, libc::S_IFREG | 0o600));
  }

  #[test]
  fn bad_values_are_rejected() {
    for bad in [
      "uid=root",
      "umask=999",
      "attr_timeout=soon",
      "attr_timeout=-1",
    ] {
      assert!(FsOptions::split(vec![bad.to_string()]).is_err(), "{bad}");
    }
  }

  #[test]
  fn flags_with_values_are_not_flags() {
    // `-o debug=1` is not libfuse syntax; it must reach the mount unchanged.
    let (fs, mount) = split(&["debug=1"]);
    assert!(!fs.debug);
    assert_eq!(mount, ["debug=1"]);
  }
}
