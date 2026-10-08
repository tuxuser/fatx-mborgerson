fatx-fuse
=========

[FUSE](https://en.wikipedia.org/wiki/Filesystem_in_Userspace) driver for the
Original Xbox FATX filesystem and the Xbox 360's XTAF filesystem, built on
[fatx](../fatx).

Mounts read-only by default. Write support (create/write/delete/rename/
mkdir/rmdir/truncate) exists but is new and unproven against real
consoles -- pass `--rw` to opt in.

```
fatx-fuse <device_path> <mount_point> [OPTIONS]
```

`device_path` is the raw device or disk image (e.g. `/dev/sdb`) containing
the console's storage -- not a single partition device node. `fatx-fuse`
locates the FATX/XTAF partition itself, at a fixed offset for known drive
types.

How-to
------------------------------

### Cross compiling for X360 Linux (PPC32 userspace)

Make sure to point SYSROOT / SR to your actual root filesystem.
In this example I am using an extracted archPOWER squashfs utilized as nfsroot.

```sh
SR=/srv/nfs/archpower                                  # PPC32 sysroot
GCCLIB=$SR/usr/lib/gcc/powerpc-unknown-linux-gnu/16    # libgcc_s.so

cd rust
CARGO_TARGET_POWERPC_UNKNOWN_LINUX_GNU_LINKER=powerpc64-linux-gnu-gcc \
CARGO_TARGET_POWERPC_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-m32 -C link-arg=--sysroot=$SR -C link-arg=-mcpu=powerpc -C link-arg=-L$GCCLIB" \
cargo build --release --target powerpc-unknown-linux-gnu -p fatx-fuse
```

### Original Xbox HDD

Select a partition with `--drive-letter` (`c`, `e`, `x`, `y`, or `z`,
matching the original Xbox's drive letters). Defaults to `c` if omitted.

```
fatx-fuse /dev/sdb /mnt/xbox --drive-letter e
```

### Xbox 360 retail HDD

Add `--xbox360`. `--drive-letter` selects the partition:

| Value     | Partition                                          |
|-----------|-----------------------------------------------------|
| `data`    | User content (games, saves, DLC) -- the "E:" equivalent. Runs to the end of the drive. |
| `compat`  | Original Xbox backwards-compatibility partition.    |

```
fatx-fuse /dev/sdb /mnt/xbox360 --xbox360 --drive-letter data
```

(System Cache / Game Cache partitions are SFCX, not FATX, and aren't
supported by this driver.)

### Xbox 360 devkit HDD

Devkits store their partition table on-disk instead of using fixed retail
offsets, so add `--devkit` alongside `--xbox360`. `--drive-letter` selects
between the two devkit-table entries:

| Value       | Partition                          |
|-------------|-------------------------------------|
| `content`   | User content volume.                |
| `dashboard` | Xbox 360 dashboard/system volume.   |

```
fatx-fuse /dev/sdb /mnt/xbox360dk --xbox360 --devkit --drive-letter content
```

### Raw disk images / loopback files

Any of the above work equally well against a plain file instead of a real
block device -- just pass the file's path as `device_path`. Useful for a
`dd`-captured image or a scratch file for testing.

Mounting read-write
--------------------

All of the above mount read-only unless you also pass `--rw`:

```
fatx-fuse /dev/sdb /mnt/xbox360 --xbox360 --drive-letter data --rw
```

Only do this against a drive you're comfortable risking write corruption
on -- consider testing against a disk image copy first.

Other flags
-----------

- `--auto-unmount` -- unmount automatically when the driver exits.
- `--allow-root` -- allow the root user to access the mount (in addition to
  the mounting user).
