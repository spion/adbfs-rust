mod adb;
mod cache;
mod escape;
mod fs;
mod mount_opts;
mod ops;
mod parse;

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use color_eyre::eyre::{Result, WrapErr};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "adbfs", about = "Mount Android device filesystem via ADB")]
struct Cli {
  /// Mount point
  mountpoint: PathBuf,

  /// Stay in the foreground instead of forking into the background
  #[arg(short = 'f', long)]
  foreground: bool,

  /// Enable debug logging (implies --foreground)
  #[arg(short = 'd', long)]
  debug: bool,

  /// Serve requests on a single thread
  #[arg(short = 's', long)]
  single_threaded: bool,

  /// Trigger Android media rescan on file changes
  #[arg(long)]
  rescan: bool,

  /// Cache TTL in seconds
  #[arg(long, default_value = "30")]
  cache_ttl: u64,

  /// Mount options: libfuse options (uid, gid, umask, entry_timeout,
  /// attr_timeout, negative_timeout, direct_io, kernel_cache, debug) are
  /// handled here, the rest are passed to the kernel
  #[arg(short = 'o', value_delimiter = ',')]
  options: Vec<String>,
}

/// Fork into the background, the way libfuse's `fuse_daemonize` does.
///
/// The mount must already be established when this runs: the parent exits
/// immediately, so anything that fails afterwards has no way to report a
/// non-zero status. It must also run before any thread is started, because
/// `fork` carries over only the calling thread.
fn daemonize() -> Result<()> {
  // SAFETY: single-threaded at this point, so the child inherits a consistent
  // address space, and everything after the fork is async-signal-safe.
  unsafe {
    match libc::fork() {
      -1 => return Err(std::io::Error::last_os_error()).wrap_err("fork failed"),
      0 => {}
      _ => libc::_exit(0),
    }

    if libc::setsid() == -1 {
      return Err(std::io::Error::last_os_error()).wrap_err("setsid failed");
    }
    // Do not pin the directory we were started from; it may itself be a mount.
    libc::chdir(c"/".as_ptr());

    let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
    if null >= 0 {
      libc::dup2(null, libc::STDIN_FILENO);
      libc::dup2(null, libc::STDOUT_FILENO);
      libc::dup2(null, libc::STDERR_FILENO);
      if null > libc::STDERR_FILENO {
        libc::close(null);
      }
    }
  }
  Ok(())
}

fn main() -> Result<()> {
  color_eyre::install()?;

  let cli = Cli::parse();
  let (fs_opts, mount_options) = mount_opts::FsOptions::split(cli.options)?;

  // `-d` and `-o debug` are the same switch, and both imply foreground.
  let debug = cli.debug || fs_opts.debug;
  let foreground = cli.foreground || debug;
  let default_filter = if debug { "adbfs=debug" } else { "adbfs=warn" };
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.parse().unwrap()),
    )
    .init();

  let adb = Arc::new(adb::cli::AdbCli::new(None));
  let device_ops =
    Arc::new(ops::DeviceOps::new(adb, ops::ResolvedCompat::legacy()).with_rescan(cli.rescan));

  let adbfs = fs::AdbFs::new(device_ops, Duration::from_secs(cli.cache_ttl), fs_opts)?;

  fs::mount(
    adbfs,
    &cli.mountpoint,
    mount_options,
    cli.single_threaded,
    || if foreground { Ok(()) } else { daemonize() },
  )?;
  Ok(())
}
