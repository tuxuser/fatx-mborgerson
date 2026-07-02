use chrono::NaiveDate;
use std::ffi::OsStr;
use std::io::{Read, Seek, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use bimap::BiMap;
use clap::Parser;
use clap::builder::styling::{AnsiColor, Effects, Style, Styles};
use fatx::{DirectoryEntry, Error, FatxFs, FatxFsConfig, FatxFsHandle, Xbox360DevkitPartition};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyWrite, Request, TimeOrNow,
};
use libc::ENOENT;

type Inode = u64;

struct InodeTracker {
    bimap: Mutex<BiMap<Inode, String>>,
    next_inode: Mutex<Inode>,
}

impl InodeTracker {
    fn new() -> Self {
        let mut bimap = BiMap::new();
        bimap.insert(1, String::from("/")); // root
        Self {
            bimap: Mutex::new(bimap),
            next_inode: Mutex::new(2),
        }
    }

    fn get_or_create_inode(&self, path: &str) -> Inode {
        let mut bimap = self.bimap.lock().unwrap();
        if let Some(inode) = bimap.get_by_right(path) {
            return *inode;
        }

        let mut inode_counter = self.next_inode.lock().unwrap();
        let inode = *inode_counter;
        *inode_counter += 1;

        bimap.insert(inode, path.to_string());
        inode
    }

    fn get_path(&self, inode: Inode) -> Option<String> {
        let bimap = self.bimap.lock().unwrap();
        bimap.get_by_left(&inode).cloned()
    }

    /// Rewrites every cached path under `old_path` (itself, and any
    /// descendants if it was a directory) to live under `new_path`
    /// instead, preserving each entry's inode number. Without this, a
    /// cached inode for a path that no longer exists after a rename would
    /// make the very next `getattr`/`lookup` on it fail.
    fn rename_path(&self, old_path: &str, new_path: &str) {
        let mut bimap = self.bimap.lock().unwrap();
        let old_prefix = format!("{old_path}/");
        let affected: Vec<(Inode, String)> = bimap
            .iter()
            .filter(|(_, path)| *path == old_path || path.starts_with(&old_prefix))
            .map(|(inode, path)| (*inode, path.clone()))
            .collect();
        for (inode, path) in affected {
            let new_path_for_inode = if path == old_path {
                new_path.to_string()
            } else {
                format!("{new_path}{}", &path[old_path.len()..])
            };
            bimap.remove_by_left(&inode);
            bimap.insert(inode, new_path_for_inode);
        }
    }

    /// Drops cached entries for `path` and any descendants -- used when a
    /// non-exchange rename overwrites an existing destination, which
    /// deletes it outright rather than moving it anywhere.
    fn forget_path(&self, path: &str) {
        let mut bimap = self.bimap.lock().unwrap();
        let prefix = format!("{path}/");
        let affected: Vec<Inode> = bimap
            .iter()
            .filter(|(_, p)| *p == path || p.starts_with(&prefix))
            .map(|(inode, _)| *inode)
            .collect();
        for inode in affected {
            bimap.remove_by_left(&inode);
        }
    }

    /// Swaps the cached paths (and any descendants) between `a` and `b` --
    /// used for `rename`'s `exchange` semantics, where both sides keep
    /// existing but their content (and inode identity) trades places.
    /// Implemented as two `rename_path` calls through a sentinel that
    /// can't collide with a real virtual path, reusing its prefix-aware
    /// rewriting for both sides.
    fn swap_paths(&self, a: &str, b: &str) {
        let sentinel = format!("\0exchange{a}");
        self.rename_path(a, &sentinel);
        self.rename_path(b, a);
        self.rename_path(&sentinel, b);
    }
}

struct FuseFatxFs {
    fatx: FatxFsHandle,
    inodes: InodeTracker,
}

fn fatx_datetime_to_systemtime(datetime: fatx::DateTime) -> SystemTime {
    if let Some(date) = NaiveDate::from_ymd_opt(
        datetime.year().into(),
        datetime.month().into(),
        datetime.day().into(),
    ) && let Some(datetime) = date.and_hms_opt(
        datetime.hour().into(),
        datetime.minute().into(),
        datetime.second().into(),
    ) {
        return SystemTime::from(datetime.and_utc());
    }

    // Failed to convert datetime. Supply default.
    let datetime = NaiveDate::from_ymd_opt(2000, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    SystemTime::from(datetime.and_utc())
}

impl FuseFatxFs {
    fn dirent_to_attr(&mut self, inode: u64, dirent: &DirectoryEntry) -> Option<FileAttr> {
        if dirent.is_directory() {
            return Some(FileAttr {
                ino: inode,
                size: 0,
                blocks: 0,
                atime: fatx_datetime_to_systemtime(dirent.accessed()),
                mtime: fatx_datetime_to_systemtime(dirent.modified()),
                ctime: fatx_datetime_to_systemtime(dirent.modified()),
                crtime: fatx_datetime_to_systemtime(dirent.created()),
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: 501,
                gid: 20,
                rdev: 0,
                flags: 0,
                blksize: 1,
            });
        }

        if dirent.is_file() {
            return Some(FileAttr {
                ino: inode,
                size: dirent.file_size() as u64,
                blocks: (dirent.file_size() / 512) as u64, // FIXME: num clusters?
                atime: fatx_datetime_to_systemtime(dirent.accessed()),
                mtime: fatx_datetime_to_systemtime(dirent.modified()),
                ctime: fatx_datetime_to_systemtime(dirent.modified()),
                crtime: fatx_datetime_to_systemtime(dirent.created()),
                kind: FileType::RegularFile,
                perm: 0o644, // FIXME
                nlink: 1,
                uid: 501,
                gid: 20,
                rdev: 0,
                flags: 0,
                blksize: 512,
            });
        }

        None
    }
}

const TTL: Duration = Duration::from_secs(1);

fn child_path(parent_dir: &str, name: &OsStr) -> String {
    let mut path = PathBuf::from(parent_dir);
    path.push(name.to_str().unwrap());
    path.to_str().unwrap().to_string()
}

/// Maps a `fatx::Error` to the errno FUSE should reply with. An explicit
/// match rather than a round-trip through `io::Error::raw_os_error()`,
/// since `io::Error::new(kind, _)` produces the `Custom` variant, whose
/// `raw_os_error()` is always `None` regardless of kind.
fn error_to_errno(err: &Error) -> i32 {
    match err {
        Error::NotFound => libc::ENOENT,
        Error::NotADirectory => libc::ENOTDIR,
        Error::IsADirectory => libc::EISDIR,
        Error::FilesystemFull => libc::ENOSPC,
        Error::DirectoryNotEmpty => libc::ENOTEMPTY,
        Error::AlreadyExists => libc::EEXIST,
        Error::InvalidFilename | Error::InvalidRenameFlags | Error::RenameTypeMismatch => {
            libc::EINVAL
        }
        Error::Io(io_err) => io_err.raw_os_error().unwrap_or(libc::EIO),
        _ => libc::EIO,
    }
}

fn to_fatx_datetime(t: TimeOrNow) -> fatx::DateTime {
    fatx::DateTime::from_system_time(match t {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => SystemTime::now(),
    })
}

impl Filesystem for FuseFatxFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        log::debug!("lookup({}, {:?})", parent, name);
        if let Some(root) = self.inodes.get_path(parent) {
            // Construct combined path
            let mut path = PathBuf::from(root);
            path.push(name.to_str().unwrap());
            let path_str = path.to_str().unwrap();

            if let Ok(dirent) = self.fatx.stat(path_str) {
                let inode = self.inodes.get_or_create_inode(path_str);
                if let Some(attr) = self.dirent_to_attr(inode, &dirent) {
                    reply.entry(&TTL, &attr, 0);
                    return;
                }
            }
        }
        reply.error(ENOENT);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        log::debug!("getattr({})", ino);
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(ENOENT);
        };
        match self.fatx.stat(&path) {
            Ok(dirent) => match self.dirent_to_attr(ino, &dirent) {
                Some(attr) => reply.attr(&TTL, &attr),
                None => reply.error(ENOENT),
            },
            Err(err) => reply.error(error_to_errno(&err)),
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        _size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        log::debug!("read({})", ino);
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(ENOENT);
        };
        let mut file = match self.fatx.open(&path) {
            Ok(f) => f,
            Err(err) => return reply.error(error_to_errno(&err)),
        };
        if let Err(err) = file.seek(std::io::SeekFrom::Start(offset as u64)) {
            return reply.error(err.raw_os_error().unwrap_or(libc::EIO));
        }
        let mut data = vec![0u8; _size as usize];
        match file.read(&mut data) {
            Ok(n) => reply.data(&data[..n]),
            Err(err) => reply.error(err.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        log::debug!("readdir({}, offset={})", ino, offset);

        if let Some(dir_path_str) = self.inodes.get_path(ino) {
            let dir_iter = match self.fatx.read_dir(&dir_path_str) {
                Ok(iter) => iter,
                Err(err) => return reply.error(error_to_errno(&err)),
            };
            let dir_path = PathBuf::from(dir_path_str);

            // Add self entry (.)
            let mut entries = vec![(ino, FileType::Directory, String::from("."))];

            // Add parent entry (..)
            if ino == 1 {
                entries.push((1, FileType::Directory, String::from("..")));
            } else {
                let parent_path = dir_path.parent().unwrap_or(&dir_path);
                let parent_path_str = parent_path.to_str().unwrap();
                let parent_inode = self.inodes.get_or_create_inode(parent_path_str);
                entries.push((parent_inode, FileType::Directory, String::from("..")));
            }

            // Add directory entries
            for dirent in dir_iter.flatten() {
                if dirent.is_file() || dirent.is_directory() {
                    let mut child_path = dir_path.clone();
                    child_path.push(dirent.file_name());
                    let child_path_str = child_path.to_str().unwrap();

                    let child_inode = self.inodes.get_or_create_inode(child_path_str);

                    let ftype = if dirent.is_file() {
                        FileType::RegularFile
                    } else {
                        FileType::Directory
                    };

                    entries.push((child_inode, ftype, dirent.file_name()))
                }
            }

            // Reply with desired entries
            for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                // i + 1 means the index of the next entry
                if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
                    break;
                }
            }
            reply.ok();
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        log::debug!("write({})", ino);
        let Some(path) = self.inodes.get_path(ino) else {
            reply.error(ENOENT);
            return;
        };
        let mut file = match self.fatx.open(&path) {
            Ok(f) => f,
            Err(err) => return reply.error(error_to_errno(&err)),
        };
        if let Err(err) = file.seek(std::io::SeekFrom::Start(offset as u64)) {
            return reply.error(err.raw_os_error().unwrap_or(libc::EIO));
        }
        match file.write(data) {
            Ok(n) => reply.written(n as u32),
            Err(err) => reply.error(err.raw_os_error().unwrap_or(libc::EIO)),
        }
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        log::debug!("create({}, {:?})", parent, name);
        let Some(parent_path) = self.inodes.get_path(parent) else {
            return reply.error(ENOENT);
        };
        let path = child_path(&parent_path, name);
        if let Err(err) = self.fatx.create_file(&path) {
            return reply.error(error_to_errno(&err));
        }
        match self.fatx.stat(&path) {
            Ok(dirent) => {
                let inode = self.inodes.get_or_create_inode(&path);
                match self.dirent_to_attr(inode, &dirent) {
                    Some(attr) => reply.created(&TTL, &attr, 0, 0, 0),
                    None => reply.error(libc::EIO),
                }
            }
            Err(err) => reply.error(error_to_errno(&err)),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        log::debug!("mkdir({}, {:?})", parent, name);
        let Some(parent_path) = self.inodes.get_path(parent) else {
            return reply.error(ENOENT);
        };
        let path = child_path(&parent_path, name);
        if let Err(err) = self.fatx.mkdir(&path) {
            return reply.error(error_to_errno(&err));
        }
        match self.fatx.stat(&path) {
            Ok(dirent) => {
                let inode = self.inodes.get_or_create_inode(&path);
                match self.dirent_to_attr(inode, &dirent) {
                    Some(attr) => reply.entry(&TTL, &attr, 0),
                    None => reply.error(libc::EIO),
                }
            }
            Err(err) => reply.error(error_to_errno(&err)),
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        log::debug!("unlink({}, {:?})", parent, name);
        let Some(parent_path) = self.inodes.get_path(parent) else {
            return reply.error(ENOENT);
        };
        let path = child_path(&parent_path, name);
        match self.fatx.unlink(&path) {
            Ok(()) => {
                self.inodes.forget_path(&path);
                reply.ok();
            }
            Err(err) => reply.error(error_to_errno(&err)),
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        log::debug!("rmdir({}, {:?})", parent, name);
        let Some(parent_path) = self.inodes.get_path(parent) else {
            return reply.error(ENOENT);
        };
        let path = child_path(&parent_path, name);
        match self.fatx.rmdir(&path) {
            Ok(()) => {
                self.inodes.forget_path(&path);
                reply.ok();
            }
            Err(err) => reply.error(error_to_errno(&err)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        log::debug!(
            "rename({}, {:?} -> {}, {:?})",
            parent,
            name,
            newparent,
            newname
        );
        let (Some(parent_path), Some(newparent_path)) = (
            self.inodes.get_path(parent),
            self.inodes.get_path(newparent),
        ) else {
            return reply.error(ENOENT);
        };
        let from = child_path(&parent_path, name);
        let to = child_path(&newparent_path, newname);
        let exchange = flags & libc::RENAME_EXCHANGE != 0;
        let no_replace = flags & libc::RENAME_NOREPLACE != 0;
        match self.fatx.rename(&from, &to, exchange, no_replace) {
            Ok(()) => {
                if exchange {
                    self.inodes.swap_paths(&from, &to);
                } else {
                    self.inodes.forget_path(&to);
                    self.inodes.rename_path(&from, &to);
                }
                reply.ok();
            }
            Err(err) => reply.error(error_to_errno(&err)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        log::debug!("setattr({})", ino);
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(ENOENT);
        };

        if let Some(size) = size {
            let mut file = match self.fatx.open(&path) {
                Ok(f) => f,
                Err(err) => return reply.error(error_to_errno(&err)),
            };
            if let Err(err) = file.set_len(size as u32) {
                return reply.error(error_to_errno(&err));
            }
        }

        if atime.is_some() || mtime.is_some() {
            let accessed = atime.map(to_fatx_datetime);
            let modified = mtime.map(to_fatx_datetime);
            if let Err(err) = self.fatx.set_times(&path, accessed, modified) {
                return reply.error(error_to_errno(&err));
            }
        }

        match self.fatx.stat(&path) {
            Ok(dirent) => match self.dirent_to_attr(ino, &dirent) {
                Some(attr) => reply.attr(&TTL, &attr),
                None => reply.error(libc::EIO),
            },
            Err(err) => reply.error(error_to_errno(&err)),
        }
    }

    fn flush(&mut self, _req: &Request, _ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        // Write-through design: nothing is buffered, so there's nothing to
        // flush -- just acknowledge.
        reply.ok();
    }

    fn fsync(&mut self, _req: &Request, _ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        match self.fatx.sync() {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(error_to_errno(&err)),
        }
    }
}

const HEADER: Style = AnsiColor::Green.on_default().effects(Effects::BOLD);
const USAGE: Style = AnsiColor::Green.on_default().effects(Effects::BOLD);
const LITERAL: Style = AnsiColor::Cyan.on_default().effects(Effects::BOLD);
const PLACEHOLDER: Style = AnsiColor::Cyan.on_default();
const ERROR: Style = AnsiColor::Red.on_default().effects(Effects::BOLD);
const VALID: Style = AnsiColor::Cyan.on_default().effects(Effects::BOLD);
const INVALID: Style = AnsiColor::Yellow.on_default().effects(Effects::BOLD);

/// Cargo's color style
/// [source](https://github.com/crate-ci/clap-cargo/blob/master/src/style.rs)
const CARGO_STYLING: Styles = Styles::styled()
    .header(HEADER)
    .usage(USAGE)
    .literal(LITERAL)
    .placeholder(PLACEHOLDER)
    .error(ERROR)
    .valid(VALID)
    .invalid(INVALID);

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
#[clap(styles = CARGO_STYLING)]
struct Cli {
    /// Device file containing FATX partition to mount
    #[arg()]
    device_path: String,

    /// FUSE filesystem mount point
    #[arg()]
    mount_point: String,

    /// Partition to mount. For the original Xbox: c|e|x|y|z. For an Xbox
    /// 360 (--xbox360): data|compat (retail), or content|dashboard
    /// (--xbox360 --devkit).
    #[arg(short, long, default_value_t = String::from("c"))]
    drive_letter: String,

    /// Target an Xbox 360 (XTAF) volume instead of the original Xbox
    #[arg(long)]
    xbox360: bool,

    /// With --xbox360, read the on-disk devkit partition table instead of
    /// using fixed retail offsets
    #[arg(long)]
    devkit: bool,

    /// Auto-unmount
    #[arg(long)]
    auto_unmount: bool,

    /// Allow root
    #[arg(long)]
    allow_root: bool,

    /// Mount read-write (default: read-only). Write support is new and
    /// hasn't been validated against a real console -- only pass this for
    /// a drive you're comfortable risking write corruption on.
    #[arg(long)]
    rw: bool,
}

fn xbox360_config(device_path: String, drive_letter: &str, devkit: bool) -> FatxFsConfig {
    if devkit {
        let partition = match drive_letter {
            "content" => Xbox360DevkitPartition::Content,
            "dashboard" => Xbox360DevkitPartition::Dashboard,
            other => panic!("invalid devkit partition '{other}', expected content|dashboard"),
        };
        FatxFsConfig::xbox360_devkit(device_path, partition)
    } else {
        match drive_letter {
            "data" => FatxFsConfig::xbox360_retail_data(device_path),
            "compat" => FatxFsConfig::xbox360_retail_compat(device_path),
            other => panic!("invalid retail partition '{other}', expected data|compat"),
        }
    }
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();
    let mut options = vec![MountOption::FSName("fatx".to_string())];
    if !cli.rw {
        options.push(MountOption::RO);
    }
    if cli.auto_unmount {
        options.push(MountOption::AutoUnmount);
    }
    if cli.allow_root {
        options.push(MountOption::AllowRoot);
    }

    let device_path = cli.device_path.clone();
    let config = if cli.xbox360 {
        xbox360_config(cli.device_path, &cli.drive_letter, cli.devkit)
    } else {
        FatxFsConfig::new(cli.device_path).drive_letter(&cli.drive_letter)
    };
    let config = if cli.rw { config.writable() } else { config };

    let fatx = FatxFs::open_device(&config).unwrap_or_else(|err| {
        eprintln!("error: failed to open '{device_path}': {err}");
        std::process::exit(1);
    });
    let fs = FuseFatxFs {
        fatx,
        inodes: InodeTracker::new(),
    };
    if let Err(err) = fuser::mount2(fs, &cli.mount_point, &options) {
        eprintln!("error: failed to mount at '{}': {err}", cli.mount_point);
        std::process::exit(1);
    }
}
