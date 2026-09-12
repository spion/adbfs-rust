## adbfs-rust (rootless)

This is an AI-assisted (but human reviewed) port of adbfs-rootless to Rust.

It continues to work without root accesss or busybox.

# Instructions:

## Ubuntu

You will need the Rust toolchain, `libfuse3-dev`, and `adb`.

    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
    sudo apt-get install libfuse3-dev android-tools-adb

## MacOS

Install adb and fuse

    brew install --cask android-platform-tools
    brew install --cask macfuse

Check access to phone through adb

    adb devices

## The rest

Clone the repository:

    git clone git@github.com:spion/adbfs-rootless.git
    cd adbfs-rootless

Build:

    cargo build --release

The binary will be at `target/release/adbfs`.

Create a mount point if needed (e.g. in your home directory):

    mkdir ~/droid

You can now mount your device:

    ./target/release/adbfs ~/droid

If you want to trigger a media rescan after every operation:

    ./target/release/adbfs --rescan ~/droid

To set cache TTL (default 30 seconds):

    ./target/release/adbfs --cache-ttl 60 ~/droid

adbfs forks into the background once the mount is up, so the command returns
immediately. Pass `-f` to keep it in the foreground, and `-d` to also turn on
debug logging (`-d` implies `-f`):

    ./target/release/adbfs -d ~/droid

Have fun!

## Mount options

| Flag                 | Meaning                                                |
| -------------------- | ------------------------------------------------------ |
| `-f`                 | Stay in the foreground                                 |
| `-d`                 | Debug logging, implies `-f` (same as `-o debug`)       |
| `-s`                 | Serve requests on a single thread                      |
| `--rescan`           | Trigger an Android media rescan after write operations |
| `--cache-ttl <secs>` | Lifetime of adbfs' own metadata cache (default 30)     |

`-o` takes the usual comma-separated list. adbfs handles the options below
itself, and passes everything else (`allow_other`, `ro`, `max_read=`, ...) to
the kernel:

| Option               | Default       | Meaning                                       |
| -------------------- | ------------- | --------------------------------------------- |
| `uid=N`, `gid=N`     | device values | Report every file as owned by N               |
| `umask=M`            | device values | Report permissions as `0777 & ~M`             |
| `entry_timeout=S`    | 1             | How long the kernel may cache a name lookup   |
| `attr_timeout=S`     | 1             | How long the kernel may cache file attributes |
| `negative_timeout=S` | 0             | How long the kernel may cache "no such file"  |
| `direct_io`          | off           | Bypass the kernel page cache for file data    |
| `kernel_cache`       | off           | Keep cached file data across opens            |
| `debug`              | off           | Same as `-d`                                  |

These are the options libfuse used to implement for every filesystem. adbfs
uses a low-level FUSE binding, so it implements them itself.

The timeouts are seconds and accept fractions. They only bound how stale the
kernel's view may be; `--cache-ttl` separately bounds how often adbfs re-reads
the device.

## Troubleshooting

### Error: device not found

When running you get the following error:

```
--*-- exec_command: adb shell ls
error: device not found
```

Solution: Make sure that [USB Debugging is enabled][enable-usb-debug].

Then `fusermount -u /media/mount/path` before trying again. Note that if for any reason `fusermount` is not available in your system, you can use `sudo umount /media/mount/path` instead.

### Error: device offline

When running you get the following error:

```
--*-- exec_command: adb shell ls
error: device offline
```

Solution: Make sure that

1. Your android-sdk-tools are up to date. Newer versions
   of Android also require newer versions of adb. For more info, see
   [this Stack Overflow post][error-device-offline].

2. You answer `Yes` when your phone asks whether it should allow the
   computer with the specified RSA key to access the device.

Then `killall -9 adb; fusermount -u /media/mount/path` before trying again.

[enable-usb-debug]: http://www.droidviews.com/how-to-enable-developer-optionsusb-debugging-mode-on-devices-with-android-4-2-jelly-bean/
[error-device-offline]: http://stackoverflow.com/questions/10680417/error-device-offline
