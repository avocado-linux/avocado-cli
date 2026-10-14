//! Content-addressed layer store for Avocado build state (the composefs model).
//!
//! On disk (see README.md):
//!   objects/ab/cdef…     file contents by sha256: the only copy of any file
//!   layers/<key>.dump    canonical composefs dump: the full tree (paths, modes, owners, xattrs → objects)
//!   layers/<key>.erofs   that dump compiled by `mkcomposefs --from-file`
//!   layers/<key>.json    the record; its presence is the commit point
//!   views/<name>/{upper,work,merged} + views/<name>.json   a mounted view = a lease on its layer
//!   locks/<key>.lock     per-key build lock (flock)
//!   pins/<name>          layer keys a project pins against GC
//!   gc.lock              writers hold it shared, GC exclusive
//!
//! Everything here runs as root inside the privileged helper container.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub mod oci;
pub mod plan;

/// Files up to this size are inlined into the EROFS image instead of becoming objects.
pub const INLINE_MAX: u64 = 64;
/// The layer every view starts from: an empty root directory.
pub const EMPTY_KEY: &str = "empty";

const S_IFMT: u32 = libc::S_IFMT;
const S_IFREG: u32 = libc::S_IFREG;
const S_IFDIR: u32 = libc::S_IFDIR;
const S_IFLNK: u32 = libc::S_IFLNK;

/// One line of a composefs dump.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub path: Vec<u8>,
    pub size: u64,
    pub mode: u32,
    pub hardlink: bool,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub mtime: (i64, u32),
    pub payload: Option<Vec<u8>>,
    pub content: Option<Vec<u8>>,
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Entry {
    pub fn is_reg(&self) -> bool {
        self.mode & S_IFMT == S_IFREG && !self.hardlink
    }
    /// The object this entry's data lives in, if any.
    pub fn object(&self) -> Option<&str> {
        if !self.is_reg() {
            return None;
        }
        self.payload
            .as_deref()
            .and_then(|p| std::str::from_utf8(p).ok())
            .map(|p| p)
    }
}

fn esc(out: &mut Vec<u8>, b: &[u8], xattr_key: bool) {
    if b == b"-" {
        out.extend_from_slice(b"\\x2d");
        return;
    }
    for &c in b {
        if c == b'\\' {
            out.extend_from_slice(b"\\\\");
        } else if c <= 0x20 || c >= 0x7f || (xattr_key && c == b'=') {
            out.extend_from_slice(format!("\\x{c:02x}").as_bytes());
        } else {
            out.push(c);
        }
    }
}

fn esc_opt(out: &mut Vec<u8>, v: &Option<Vec<u8>>) {
    match v {
        None => out.push(b'-'),
        Some(b) => esc(out, b, false),
    }
}

fn unesc(s: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' {
            match s.get(i + 1) {
                Some(b'\\') => {
                    out.push(b'\\');
                    i += 2
                }
                Some(b'n') => {
                    out.push(b'\n');
                    i += 2
                }
                Some(b'r') => {
                    out.push(b'\r');
                    i += 2
                }
                Some(b't') => {
                    out.push(b'\t');
                    i += 2
                }
                Some(b'x') => {
                    let h = std::str::from_utf8(s.get(i + 2..i + 4).ok_or_else(|| anyhow!("bad escape"))?)?;
                    out.push(u8::from_str_radix(h, 16)?);
                    i += 4
                }
                _ => bail!("bad escape in dump"),
            }
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    Ok(out)
}

fn unesc_opt(s: &[u8]) -> Result<Option<Vec<u8>>> {
    if s == b"-" {
        Ok(None)
    } else {
        Ok(Some(unesc(s)?))
    }
}

pub fn write_dump(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 120);
    for e in entries {
        esc(&mut out, &e.path, false);
        let mode = if e.hardlink {
            format!("@{:o}", e.mode)
        } else {
            format!("{:o}", e.mode)
        };
        // nlink is not part of the model: mkcomposefs derives it.
        out.extend_from_slice(
            format!(
                " {} {} 1 {} {} {} {}.{} ",
                e.size, mode, e.uid, e.gid, e.rdev, e.mtime.0, e.mtime.1
            )
            .as_bytes(),
        );
        esc_opt(&mut out, &e.payload);
        out.push(b' ');
        esc_opt(&mut out, &e.content);
        out.extend_from_slice(b" -");
        for (k, v) in &e.xattrs {
            out.push(b' ');
            esc(&mut out, k, true);
            out.push(b'=');
            esc(&mut out, v, false);
        }
        out.push(b'\n');
    }
    out
}

pub fn parse_dump(data: &[u8]) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for line in data.split(|&c| c == b'\n') {
        if line.is_empty() {
            continue;
        }
        let f: Vec<&[u8]> = line.split(|&c| c == b' ').collect();
        if f.len() < 11 {
            bail!("short dump line: {}", String::from_utf8_lossy(line));
        }
        let num = |b: &[u8]| -> Result<u64> { Ok(std::str::from_utf8(b)?.parse()?) };
        let (hardlink, mode_s) = match f[2].strip_prefix(b"@") {
            Some(m) => (true, m),
            None => (false, f[2]),
        };
        let mt = std::str::from_utf8(f[7])?;
        let (s, ns) = mt.split_once('.').unwrap_or((mt, "0"));
        let mut xattrs = Vec::new();
        for x in &f[11..] {
            let pos = x.iter().position(|&c| c == b'=').ok_or_else(|| anyhow!("bad xattr"))?;
            xattrs.push((unesc(&x[..pos])?, unesc(&x[pos + 1..])?));
        }
        entries.push(Entry {
            path: unesc(f[0])?,
            size: num(f[1])?,
            mode: u32::from_str_radix(std::str::from_utf8(mode_s)?, 8)?,
            hardlink,
            uid: num(f[4])? as u32,
            gid: num(f[5])? as u32,
            rdev: num(f[6])?,
            mtime: (s.parse()?, ns.parse()?),
            payload: unesc_opt(f[8])?,
            content: unesc_opt(f[9])?,
            xattrs,
        });
    }
    Ok(entries)
}

/// A name unique across concurrent helpers: every helper is PID 1 in its own container and, with
/// --network=host, shares the host's hostname, so a random per-process tag plus a counter.
pub fn uniq() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let t = TAG.get_or_init(|| {
        let mut b = [0u8; 6];
        let _ = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
        b.iter().map(|x| format!("{x:02x}")).collect()
    });
    format!("{t}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

pub fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("path with NUL")
}

/// xattrs worth keeping: everything but SELinux labels (host policy, not content) and overlay's own.
fn keep_xattr(k: &[u8]) -> bool {
    !(k == b"security.selinux" || k.starts_with(b"trusted.overlay."))
}

fn list_xattrs(p: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let c = cstr(p);
    let mut buf = vec![0u8; 4096];
    let n = unsafe { libc::llistxattr(c.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOTSUP) {
            return Ok(Vec::new());
        }
        return Err(err).with_context(|| format!("llistxattr {}", p.display()));
    }
    let mut out = Vec::new();
    for name in buf[..n as usize].split(|&c| c == 0).filter(|n| !n.is_empty()) {
        if !keep_xattr(name) {
            continue;
        }
        out.push((name.to_vec(), get_xattr(p, name)?.unwrap_or_default()));
    }
    out.sort();
    Ok(out)
}

pub fn get_xattr(p: &Path, name: &[u8]) -> Result<Option<Vec<u8>>> {
    let c = cstr(p);
    let cn = CString::new(name)?;
    let mut buf = vec![0u8; 65536];
    let n = unsafe {
        libc::lgetxattr(c.as_ptr(), cn.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, buf.len())
    };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(libc::ENODATA) | Some(libc::ENOTSUP)) {
            return Ok(None);
        }
        return Err(err).with_context(|| format!("lgetxattr {}", p.display()));
    }
    buf.truncate(n as usize);
    Ok(Some(buf))
}

/// Where a regular file's data comes from during an import or commit.
enum Src {
    /// Read and hash this file; it may be moved into objects/ (it is about to be discarded).
    Movable(PathBuf),
    /// Read and hash this file; copy it into objects/.
    Copy(PathBuf),
}

/// Reuse decision for an unchanged file: the parent's (payload, content).
type Reuse = (Option<Vec<u8>>, Option<Vec<u8>>);

struct Walk {
    entries: Vec<Entry>,
    todo: Vec<(usize, Src)>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Walk `root` depth-first with siblings sorted by name (the canonical order), skipping the
/// top-level names in `exclude`. `decide` returns Some(reuse) for regular files whose data is
/// unchanged from the parent layer, or the source to hash.
fn walk(
    root: &Path,
    exclude: &HashSet<Vec<u8>>,
    decide: &dyn Fn(&[u8], &fs::Metadata, &Path) -> std::result::Result<Reuse, Src>,
) -> Result<Walk> {
    let mut w = Walk { entries: Vec::new(), todo: Vec::new() };
    let mut stack: Vec<(PathBuf, Vec<u8>)> = vec![(root.to_path_buf(), b"/".to_vec())];
    while let Some((full, path)) = stack.pop() {
        let md = fs::symlink_metadata(&full).with_context(|| format!("lstat {}", full.display()))?;
        let mode = md.mode();
        let ft = mode & S_IFMT;
        let mut e = Entry {
            path: path.clone(),
            size: 0,
            mode,
            hardlink: false,
            uid: md.uid(),
            gid: md.gid(),
            rdev: md.rdev(),
            mtime: (md.mtime(), md.mtime_nsec() as u32),
            payload: None,
            content: None,
            xattrs: list_xattrs(&full)?,
        };
        // Hardlinks are not modelled: overlay shows lower hardlinks as separate inodes anyway,
        // and objects already dedup the data. Each link is a regular file with the same object.
        match ft {
            S_IFREG => {
                e.size = md.size();
                if e.size > 0 {
                    match decide(&path, &md, &full) {
                        Ok((p, c)) => {
                            e.payload = p;
                            e.content = c;
                        }
                        Err(src) => w.todo.push((w.entries.len(), src)),
                    }
                }
                w.entries.push(e);
            }
            S_IFLNK => {
                let t = fs::read_link(&full)?;
                e.size = t.as_os_str().len() as u64;
                e.payload = Some(t.as_os_str().as_bytes().to_vec());
                w.entries.push(e);
            }
            S_IFDIR => {
                w.entries.push(e);
                let mut kids: Vec<_> = fs::read_dir(&full).with_context(|| format!("readdir {}", full.display()))?
                    .map(|d| d.map(|d| d.file_name()))
                    .collect::<std::io::Result<_>>()?;
                kids.sort();
                // Reverse so the stack pops them in sorted order.
                for k in kids.into_iter().rev() {
                    let kb = k.as_bytes();
                    if path == b"/" && exclude.contains(kb) {
                        continue;
                    }
                    let mut kp = path.clone();
                    if kp != b"/" {
                        kp.push(b'/');
                    }
                    kp.extend_from_slice(kb);
                    stack.push((full.join(&k), kp));
                }
            }
            _ => w.entries.push(e),
        }
    }
    Ok(w)
}

#[derive(Default)]
pub struct HashStats {
    pub hashed_files: AtomicU64,
    pub hashed_bytes: AtomicU64,
    pub new_objects: AtomicU64,
    pub new_bytes: AtomicU64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LayerRecord {
    pub key: String,
    pub parent: Option<String>,
    pub tree_digest: String,
    pub created: u64,
    pub producer: String,
    pub inputs: serde_json::Value,
    pub files: u64,
    pub objects: u64,
    pub logical_bytes: u64,
    pub new_objects: u64,
    pub new_bytes: u64,
    pub image_bytes: u64,
    pub secs: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ViewRecord {
    pub name: String,
    pub layer: String,
    /// Top-level names that stay in the view's upper and never enter a layer.
    pub private: Vec<String>,
    pub updated: u64,
    /// Where the view is mounted, when not `views/<name>/merged` (e.g. inside a docker
    /// volume's `_data/<target>`, so every existing `-v avo-…:/opt/_avocado` sees it).
    #[serde(default)]
    pub at: Option<PathBuf>,
}

pub struct Store {
    pub root: PathBuf,
}

/// A held flock; released on drop.
pub struct Flock(#[allow(dead_code)] File);

impl Flock {
    pub fn take(path: &Path, exclusive: bool) -> Result<Flock> {
        let f = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open lock {}", path.display()))?;
        let op = if exclusive { libc::LOCK_EX } else { libc::LOCK_SH };
        if unsafe { libc::flock(f.as_raw_fd(), op) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Flock(f))
    }
}

pub fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output().with_context(|| format!("spawn {cmd:?}"))?;
    if !out.status.success() {
        bail!("{cmd:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn mount_erofs(image: &Path, at: &Path) -> Result<()> {
    let (src, dst) = (cstr(image), cstr(at));
    let fstype = CString::new("erofs").unwrap();
    let r = unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), fstype.as_ptr(), libc::MS_RDONLY, std::ptr::null()) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("mount erofs {}", image.display()));
    }
    Ok(())
}

pub fn is_mounted(p: &Path) -> bool {
    let target = p.as_os_str().as_bytes();
    fs::read("/proc/self/mountinfo")
        .map(|mi| {
            mi.split(|&c| c == b'\n')
                .any(|l| l.split(|&c| c == b' ').nth(4) == Some(target))
        })
        .unwrap_or(false)
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", crate::uniq()));
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn rm_rf(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p).with_context(|| format!("rm -r {}", p.display())),
        Ok(_) => Ok(fs::remove_file(p)?),
        Err(_) => Ok(()),
    }
}

fn threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(32)
}

impl Store {
    pub fn open(root: &Path) -> Result<Store> {
        for d in ["objects", "layers", "views", "locks", "pins", "tmp"] {
            fs::create_dir_all(root.join(d))?;
        }
        let s = Store { root: root.to_path_buf() };
        if !s.has_layer(EMPTY_KEY) {
            let root_entry = Entry {
                path: b"/".to_vec(),
                size: 0,
                mode: S_IFDIR | 0o755,
                hardlink: false,
                uid: 0,
                gid: 0,
                rdev: 0,
                mtime: (0, 0),
                payload: None,
                content: None,
                xattrs: vec![],
            };
            s.write_layer(EMPTY_KEY, None, &[root_entry], serde_json::json!({}), (0, 0), 0.0)?;
        }
        Ok(s)
    }

    pub fn gc_lock(&self, exclusive: bool) -> Result<Flock> {
        Flock::take(&self.root.join("gc.lock"), exclusive)
    }

    pub fn key_lock(&self, key: &str) -> Result<Flock> {
        Flock::take(&self.root.join("locks").join(format!("{key}.lock")), true)
    }

    pub fn obj_path(&self, hex: &str) -> PathBuf {
        self.root.join("objects").join(&hex[..2]).join(&hex[2..])
    }

    fn layer_file(&self, key: &str, ext: &str) -> PathBuf {
        self.root.join("layers").join(format!("{key}.{ext}"))
    }

    pub fn has_layer(&self, key: &str) -> bool {
        self.layer_file(key, "json").exists()
    }

    pub fn record(&self, key: &str) -> Result<LayerRecord> {
        let p = self.layer_file(key, "json");
        Ok(serde_json::from_slice(&fs::read(&p).with_context(|| format!("no layer {key}"))?)?)
    }

    pub fn dump_bytes(&self, key: &str) -> Result<Vec<u8>> {
        fs::read(self.layer_file(key, "dump")).with_context(|| format!("no dump for layer {key}"))
    }

    pub fn dump(&self, key: &str) -> Result<Vec<Entry>> {
        parse_dump(&self.dump_bytes(key)?)
    }

    pub fn erofs_path(&self, key: &str) -> PathBuf {
        self.layer_file(key, "erofs")
    }

    pub fn layers(&self) -> Result<Vec<String>> {
        let mut v: Vec<String> = fs::read_dir(self.root.join("layers"))?
            .filter_map(|d| d.ok())
            .filter_map(|d| d.file_name().to_str().and_then(|n| n.strip_suffix(".json")).map(String::from))
            // An alias whose target was evicted dangles: it is not a layer (GC removes it).
            .filter(|k| self.has_layer(k))
            .collect();
        v.sort();
        Ok(v)
    }

    /// Mark a layer used now (LRU input for eviction).
    pub fn touch(&self, key: &str) {
        let p = cstr(&self.layer_file(key, "json"));
        unsafe { libc::utimensat(libc::AT_FDCWD, p.as_ptr(), std::ptr::null(), 0) };
    }

    /// Put `src` into objects/ under `hex`. `movable` sources are renamed in (same filesystem),
    /// others are reflinked when the filesystem allows it, else copied.
    fn put_object(&self, src: &Path, hex: &str, movable: bool) -> Result<bool> {
        let dst = self.obj_path(hex);
        if dst.exists() {
            return Ok(false);
        }
        fs::create_dir_all(dst.parent().unwrap())?;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = self.root.join("tmp").join(format!("obj.{hex}.{}.{}", crate::uniq(), SEQ.fetch_add(1, Ordering::Relaxed)));
        if !(movable && fs::rename(src, &tmp).is_ok()) {
            let mut i = File::open(src).with_context(|| format!("open {}", src.display()))?;
            let o = File::create(&tmp)?;
            // FICLONE: btrfs/xfs share extents; elsewhere it fails and we copy.
            const FICLONE: libc::Ioctl = 0x40049409;
            if unsafe { libc::ioctl(o.as_raw_fd(), FICLONE, i.as_raw_fd()) } != 0 {
                let mut o = o;
                std::io::copy(&mut i, &mut o)?;
            }
        }
        fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
        let c = cstr(&tmp);
        unsafe { libc::lchown(c.as_ptr(), 0, 0) };
        // Two writers of the same object race harmlessly: same bytes, last rename wins.
        fs::rename(&tmp, &dst).with_context(|| format!("store object {hex}"))?;
        Ok(true)
    }

    /// Hash (and store) every pending file of a walk, in parallel.
    fn resolve(&self, w: &mut Walk, stats: &HashStats) -> Result<()> {
        let todo = std::mem::take(&mut w.todo);
        let results: Mutex<Vec<(usize, Reuse)>> = Mutex::new(Vec::with_capacity(todo.len()));
        let next = AtomicUsize::new(0);
        let err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
        std::thread::scope(|s| {
            for _ in 0..threads() {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((idx, src)) = todo.get(i) else { break };
                    let r = (|| -> Result<Reuse> {
                        let (path, movable) = match src {
                            Src::Movable(p) => (p, true),
                            Src::Copy(p) => (p, false),
                        };
                        let size = w.entries[*idx].size;
                        if size <= INLINE_MAX {
                            return Ok((None, Some(fs::read(path)?)));
                        }
                        let hex = sha256_file(path)?;
                        stats.hashed_files.fetch_add(1, Ordering::Relaxed);
                        stats.hashed_bytes.fetch_add(size, Ordering::Relaxed);
                        if self.put_object(path, &hex, movable)? {
                            stats.new_objects.fetch_add(1, Ordering::Relaxed);
                            stats.new_bytes.fetch_add(size, Ordering::Relaxed);
                        }
                        Ok((Some(format!("{}/{}", &hex[..2], &hex[2..]).into_bytes()), None))
                    })();
                    match r {
                        Ok(v) => results.lock().unwrap().push((*idx, v)),
                        Err(e) => {
                            *err.lock().unwrap() = Some(e);
                            break;
                        }
                    }
                });
            }
        });
        if let Some(e) = err.into_inner().unwrap() {
            return Err(e);
        }
        for (idx, (p, c)) in results.into_inner().unwrap() {
            w.entries[idx].payload = p;
            w.entries[idx].content = c;
        }
        Ok(())
    }

    /// Write a layer atomically: dump + erofs first, the json record last (the commit point).
    /// If another writer committed the same key first, theirs wins and ours is dropped.
    fn write_layer(
        &self,
        key: &str,
        parent: Option<&str>,
        entries: &[Entry],
        inputs: serde_json::Value,
        new: (u64, u64),
        secs: f64,
    ) -> Result<LayerRecord> {
        if self.has_layer(key) {
            return self.record(key);
        }
        let dump = write_dump(entries);
        let pid = crate::uniq();
        let tmp_dump = self.root.join("tmp").join(format!("{key}.{pid}.dump"));
        let tmp_img = self.root.join("tmp").join(format!("{key}.{pid}.erofs"));
        fs::write(&tmp_dump, &dump)?;
        run(Command::new("mkcomposefs").arg("--from-file").arg(&tmp_dump).arg(&tmp_img))?;
        let mut objects = HashSet::new();
        let mut logical = 0;
        for e in entries {
            if e.is_reg() {
                logical += e.size;
                if let Some(o) = e.object() {
                    objects.insert(o);
                }
            }
        }
        let rec = LayerRecord {
            key: key.to_string(),
            parent: parent.map(String::from),
            tree_digest: sha256_hex(&dump),
            created: now_secs(),
            producer: format!("avocado-store {}", env!("CARGO_PKG_VERSION")),
            inputs,
            files: entries.len() as u64,
            objects: objects.len() as u64,
            logical_bytes: logical,
            new_objects: new.0,
            new_bytes: new.1,
            image_bytes: fs::metadata(&tmp_img)?.len(),
            secs,
        };
        fs::rename(&tmp_dump, self.layer_file(key, "dump"))?;
        fs::rename(&tmp_img, self.layer_file(key, "erofs"))?;
        atomic_write(&self.layer_file(key, "json"), &serde_json::to_vec_pretty(&rec)?)?;
        Ok(rec)
    }

    /// Import a directory tree as a complete layer.
    pub fn import_dir(&self, src: &Path, key: &str, inputs: serde_json::Value, exclude: &[String]) -> Result<LayerRecord> {
        let _gc = self.gc_lock(false)?;
        if self.has_layer(key) {
            return self.record(key);
        }
        let t = Instant::now();
        let ex: HashSet<Vec<u8>> = exclude.iter().map(|s| s.as_bytes().to_vec()).collect();
        let mut w = walk(src, &ex, &|_, _, full| Err(Src::Copy(full.to_path_buf())))?;
        let stats = HashStats::default();
        self.resolve(&mut w, &stats)?;
        let new = (stats.new_objects.load(Ordering::Relaxed), stats.new_bytes.load(Ordering::Relaxed));
        self.write_layer(key, None, &w.entries, inputs, new, t.elapsed().as_secs_f64())
    }

    pub fn view_dir(&self, name: &str) -> PathBuf {
        self.root.join("views").join(name)
    }

    pub fn view(&self, name: &str) -> Result<Option<ViewRecord>> {
        let p = self.root.join("views").join(format!("{name}.json"));
        if !p.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(p)?)?))
    }

    pub fn views(&self) -> Result<Vec<ViewRecord>> {
        let mut out = Vec::new();
        for d in fs::read_dir(self.root.join("views"))? {
            let n = d?.file_name();
            if let Some(name) = n.to_str().and_then(|n| n.strip_suffix(".json")) {
                if let Some(v) = self.view(name)? {
                    out.push(v);
                }
            }
        }
        Ok(out)
    }

    fn save_view(&self, v: &ViewRecord) -> Result<()> {
        atomic_write(
            &self.root.join("views").join(format!("{}.json", v.name)),
            &serde_json::to_vec_pretty(v)?,
        )
    }

    /// Mount view `name` on its recorded layer (or `layer`, for a new view). Idempotent.
    pub fn merged(&self, v: &ViewRecord) -> PathBuf {
        v.at.clone().unwrap_or_else(|| self.view_dir(&v.name).join("merged"))
    }

    pub fn mount_view(&self, name: &str, layer: Option<&str>, private: &[String]) -> Result<ViewRecord> {
        self.mount_view_at(name, layer, private, None)
    }

    pub fn mount_view_at(&self, name: &str, layer: Option<&str>, private: &[String], at: Option<&Path>) -> Result<ViewRecord> {
        // Shared GC lock: GC clears tmp/ (where the staging mount lives) and must see the lease.
        let _gc = self.gc_lock(false)?;
        let mut v = self.view(name)?.unwrap_or(ViewRecord {
            name: name.to_string(),
            layer: EMPTY_KEY.to_string(),
            private: private.to_vec(),
            updated: now_secs(),
            at: None,
        });
        if !private.is_empty() {
            v.private = private.to_vec();
        }
        if let Some(a) = at {
            v.at = Some(a.to_path_buf());
        }
        let dir = self.view_dir(name);
        let merged = self.merged(&v);
        if is_mounted(&merged) {
            if let Some(l) = layer {
                if l != v.layer {
                    bail!("view {name} is mounted on {}, not {l}; use `use` to switch", v.layer);
                }
            }
            return Ok(v);
        }
        if let Some(l) = layer {
            v.layer = l.to_string();
        }
        if !self.has_layer(&v.layer) {
            bail!("layer {} is not in this store", v.layer);
        }
        for d in ["upper", "work"] {
            fs::create_dir_all(dir.join(d))?;
        }
        fs::create_dir_all(&merged)?;
        // A fresh upper's root is what the merged root reports: give it the layer root's metadata.
        let up = dir.join("upper");
        if fs::read_dir(&up)?.next().is_none() {
            let d = self.dump_bytes(&v.layer)?;
            let first = d.split(|&c| c == b'\n').next().unwrap_or_default();
            if let Some(r) = parse_dump(first)?.first() {
                let c = cstr(&up);
                unsafe {
                    libc::chown(c.as_ptr(), r.uid, r.gid);
                    libc::chmod(c.as_ptr(), r.mode & 0o7777);
                    let ts = [libc::timespec { tv_sec: r.mtime.0, tv_nsec: r.mtime.1 as _ }; 2];
                    libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), 0);
                }
            }
        }
        // The lease is recorded before the mount, so GC never sees the mount without it.
        v.updated = now_secs();
        self.save_view(&v)?;
        self.touch(&v.layer);
        let tmp = self.root.join("tmp").join(format!("erofs.{name}.{}", crate::uniq()));
        fs::create_dir_all(&tmp)?;
        // mount(2) directly, not mount(8): the syscall autoloads erofs and mounts the image file
        // itself (EROFS_FS_BACKED_BY_FILE, kernel >= 6.12); util-linux would instead reach for a
        // loop device when the module is not loaded yet, and the helper has none.
        mount_erofs(&self.erofs_path(&v.layer), &tmp)?;
        let opts = format!(
            "lowerdir={}::{},upperdir={},workdir={},metacopy=on,redirect_dir=on",
            tmp.display(),
            self.root.join("objects").display(),
            dir.join("upper").display(),
            dir.join("work").display()
        );
        let r = run(Command::new("mount").args(["-t", "overlay", "overlay", "-o", &opts]).arg(&merged));
        // The overlay holds the erofs superblock; the staging mount can go either way.
        let _ = run(Command::new("umount").arg("-l").arg(&tmp));
        let _ = fs::remove_dir(&tmp);
        r?;
        Ok(v)
    }

    pub fn umount_view(&self, name: &str) -> Result<()> {
        let Some(v) = self.view(name)? else { return Ok(()) };
        let merged = self.merged(&v);
        if is_mounted(&merged) {
            run(Command::new("umount").arg(&merged))?;
        }
        Ok(())
    }

    /// Drop a view entirely (its upper too) and release its lease.
    pub fn drop_view(&self, name: &str) -> Result<()> {
        self.umount_view(name)?;
        rm_rf(&self.view_dir(name))?;
        rm_rf(&self.root.join("views").join(format!("{name}.json")))
    }

    /// Replace the view's upper with a fresh one, carrying its private paths over.
    fn reset_upper(&self, v: &ViewRecord) -> Result<()> {
        let dir = self.view_dir(&v.name);
        let next = dir.join("upper.next");
        rm_rf(&next)?;
        fs::create_dir_all(&next)?;
        for p in &v.private {
            let from = dir.join("upper").join(p);
            if fs::symlink_metadata(&from).is_ok() {
                fs::rename(&from, next.join(p))?;
            }
        }
        rm_rf(&dir.join("upper"))?;
        rm_rf(&dir.join("work"))?;
        fs::rename(&next, dir.join("upper"))?;
        fs::create_dir_all(dir.join("work"))?;
        Ok(())
    }

    /// Switch a view to another layer, discarding its shared-path changes (private paths stay).
    pub fn use_layer(&self, name: &str, key: &str, private: &[String]) -> Result<ViewRecord> {
        let _gc = self.gc_lock(false)?;
        if !self.has_layer(key) {
            bail!("layer {key} is not in this store");
        }
        let mut v = self.mount_view(name, None, private)?;
        self.umount_view(name)?;
        self.reset_upper(&v)?;
        v.layer = key.to_string();
        self.save_view(&v)?;
        self.mount_view(name, Some(key), &[])
    }

    /// Harvest a mounted view into layer `key` (parent = the view's current layer), then remount
    /// the view on it with a fresh upper that keeps only the private paths.
    pub fn commit_view(&self, name: &str, key: &str, inputs: serde_json::Value) -> Result<LayerRecord> {
        let _gc = self.gc_lock(false)?;
        let v = self.view(name)?.ok_or_else(|| anyhow!("no view {name}"))?;
        let dir = self.view_dir(name);
        let merged = self.merged(&v);
        if !is_mounted(&merged) {
            bail!("view {name} is not mounted");
        }
        if self.has_layer(key) {
            // Someone else produced it meanwhile: just switch to theirs.
            drop(_gc);
            self.use_layer(name, key, &[])?;
            return self.record(key);
        }
        let t = Instant::now();
        let parent = v.layer.clone();
        let pmap: HashMap<Vec<u8>, Entry> = self.dump(&parent)?.into_iter().map(|e| (e.path.clone(), e)).collect();

        // What the upper holds: files with real data, and redirected (renamed) dirs.
        let upper = dir.join("upper");
        let mut upper_data: HashSet<Vec<u8>> = HashSet::new();
        let mut redirected: Vec<Vec<u8>> = Vec::new();
        let uw = walk(&upper, &HashSet::new(), &|_, _, _| Ok((None, None)))?;
        for e in &uw.entries {
            let full = upper.join(std::ffi::OsStr::from_bytes(&e.path[1..]));
            match e.mode & S_IFMT {
                S_IFREG if get_xattr(&full, b"trusted.overlay.metacopy")?.is_none() => {
                    upper_data.insert(e.path.clone());
                }
                S_IFDIR if get_xattr(&full, b"trusted.overlay.redirect")?.is_some() => {
                    let mut p = e.path.clone();
                    p.push(b'/');
                    redirected.push(p);
                }
                _ => {}
            }
        }
        let ex: HashSet<Vec<u8>> = v.private.iter().map(|s| s.as_bytes().to_vec()).collect();
        let decide = |path: &[u8], md: &fs::Metadata, full: &Path| -> std::result::Result<Reuse, Src> {
            if upper_data.contains(path) {
                return Err(Src::Movable(upper.join(std::ffi::OsStr::from_bytes(&path[1..]))));
            }
            if redirected.iter().any(|r| path.starts_with(r)) {
                return Err(Src::Copy(full.to_path_buf()));
            }
            match pmap.get(path) {
                Some(p) if p.is_reg() && p.size == md.size() => Ok((p.payload.clone(), p.content.clone())),
                _ => Err(Src::Copy(full.to_path_buf())),
            }
        };
        let mut w = walk(&merged, &ex, &decide)?;
        // Files read through the view first; then unmount and move upper files into objects/.
        let (movable, through): (Vec<_>, Vec<_>) =
            std::mem::take(&mut w.todo).into_iter().partition(|(_, s)| matches!(s, Src::Movable(_)));
        let stats = HashStats::default();
        w.todo = through;
        self.resolve(&mut w, &stats)?;
        self.umount_view(name)?;
        w.todo = movable;
        let r = self.resolve(&mut w, &stats);
        if let Err(e) = r {
            // The upper may be half-moved: the view goes back to its parent, the step reruns.
            self.reset_upper(&v)?;
            self.mount_view(name, None, &[])?;
            return Err(e);
        }
        let new = (stats.new_objects.load(Ordering::Relaxed), stats.new_bytes.load(Ordering::Relaxed));
        let rec = self.write_layer(key, Some(&parent), &w.entries, inputs, new, t.elapsed().as_secs_f64())?;
        self.reset_upper(&v)?;
        let mut v = v;
        v.layer = key.to_string();
        self.save_view(&v)?;
        self.mount_view(name, Some(key), &[])?;
        Ok(rec)
    }

    /// Mount the layer read-only, re-hash every file through it and compare the tree digest.
    pub fn verify(&self, key: &str) -> Result<(bool, String, f64)> {
        let t = Instant::now();
        let rec = self.record(key)?;
        let name = format!("verify-{key}-{}", crate::uniq());
        self.mount_view(&name, Some(key), &[])?;
        let merged = self.view_dir(&name).join("merged");
        let r = (|| -> Result<String> {
            let mut w = walk(&merged, &HashSet::new(), &|_, _, full| Err(Src::Copy(full.to_path_buf())))?;
            // Hash without storing: every object must already exist.
            let todo = std::mem::take(&mut w.todo);
            let next = AtomicUsize::new(0);
            let res: Mutex<Vec<(usize, Reuse)>> = Mutex::new(Vec::new());
            std::thread::scope(|s| {
                for _ in 0..threads() {
                    s.spawn(|| loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some((idx, Src::Copy(p))) = todo.get(i) else { break };
                        let v = if w.entries[*idx].size <= INLINE_MAX {
                            (None, fs::read(p).ok())
                        } else {
                            let h = sha256_file(p).unwrap_or_default();
                            (Some(format!("{}/{}", &h[..2.min(h.len())], h.get(2..).unwrap_or("")).into_bytes()), None)
                        };
                        res.lock().unwrap().push((*idx, v));
                    });
                }
            });
            for (idx, (p, c)) in res.into_inner().unwrap() {
                w.entries[idx].payload = p;
                w.entries[idx].content = c;
            }
            let d = write_dump(&w.entries);
            // Kept for diffing against layers/<key>.dump when the digests disagree.
            fs::write(self.root.join("tmp").join(format!("verify-{key}.dump")), &d)?;
            Ok(sha256_hex(&d))
        })();
        self.drop_view(&name)?;
        let got = r?;
        Ok((got == rec.tree_digest, got, t.elapsed().as_secs_f64()))
    }

    /// Mount every recorded view whose name starts with `prefix` that is not mounted (after a
    /// reboot, or on a project's first command of the day).
    pub fn remount(&self, prefix: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for v in self.views()? {
            if v.name.starts_with(prefix) && !is_mounted(&self.merged(&v)) {
                self.mount_view(&v.name, None, &[])?;
                out.push(v.name);
            }
        }
        Ok(out)
    }

    /// Make `alias` another name for layer `key` (symlinked files; evicting `key` breaks it,
    /// which reads as a plain miss).
    pub fn alias(&self, alias: &str, key: &str) -> Result<()> {
        if !self.has_layer(key) || self.has_layer(alias) {
            return Ok(());
        }
        for ext in ["dump", "erofs", "json"] {
            let l = self.layer_file(alias, ext);
            let _ = fs::remove_file(&l);
            std::os::unix::fs::symlink(format!("{key}.{ext}"), &l)?;
        }
        Ok(())
    }

    pub fn pins(&self) -> Result<HashMap<String, Vec<String>>> {
        let mut out = HashMap::new();
        for d in fs::read_dir(self.root.join("pins"))? {
            let d = d?;
            let keys = fs::read_to_string(d.path())?.lines().map(String::from).collect();
            out.insert(d.file_name().to_string_lossy().to_string(), keys);
        }
        Ok(out)
    }

    pub fn pin(&self, name: &str, keys: &[String]) -> Result<()> {
        atomic_write(&self.root.join("pins").join(name), keys.join("\n").as_bytes())
    }

    pub fn unpin(&self, name: &str) -> Result<()> {
        rm_rf(&self.root.join("pins").join(name))
    }

    /// Object → size for every object on disk.
    fn object_sizes(&self) -> Result<HashMap<String, u64>> {
        let mut out = HashMap::new();
        for d in fs::read_dir(self.root.join("objects"))? {
            let d = d?;
            let pre = d.file_name().to_string_lossy().to_string();
            for f in fs::read_dir(d.path())? {
                let f = f?;
                out.insert(format!("{pre}/{}", f.file_name().to_string_lossy()), f.metadata()?.len());
            }
        }
        Ok(out)
    }

    pub fn stats(&self) -> Result<serde_json::Value> {
        let objs = self.object_sizes()?;
        let obj_bytes: u64 = objs.values().sum();
        let mut layer_bytes = 0;
        let mut logical = 0;
        let mut layers = Vec::new();
        for k in self.layers()? {
            let r = self.record(&k)?;
            layer_bytes += r.image_bytes + fs::metadata(self.layer_file(&k, "dump")).map(|m| m.len()).unwrap_or(0);
            logical += r.logical_bytes;
            layers.push(serde_json::json!({"key": k, "files": r.files, "objects": r.objects,
                "logical_bytes": r.logical_bytes, "image_bytes": r.image_bytes, "parent": r.parent,
                "inputs": r.inputs}));
        }
        let mut upper_bytes = 0;
        for v in self.views()? {
            upper_bytes += du(&self.view_dir(&v.name).join("upper"));
        }
        Ok(serde_json::json!({
            "root": self.root, "objects": objs.len(), "object_bytes": obj_bytes,
            "layer_meta_bytes": layer_bytes, "logical_bytes_all_layers": logical,
            "upper_bytes": upper_bytes, "layers": layers, "views": self.views()?, "pins": self.pins()?,
        }))
    }

    /// Evict least-recently-used layers that no pin or view references until the store is at or
    /// under `low` bytes, if it is over `high`. Then drop objects no remaining layer references.
    /// Crash-safe: records go first (a layer without its json is garbage), objects last, all
    /// under the exclusive GC lock, so no writer can be mid-way referencing an object we drop.
    pub fn gc(&self, high: u64, low: u64, dry_run: bool) -> Result<serde_json::Value> {
        let t = Instant::now();
        let _gc = self.gc_lock(true)?;
        // Orphans from crashed writers: tmp files, layer files without a record.
        rm_rf(&self.root.join("tmp"))?;
        fs::create_dir_all(self.root.join("tmp"))?;
        for d in fs::read_dir(self.root.join("layers"))? {
            let p = d?.path();
            if fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) && fs::metadata(&p).is_err() {
                if !dry_run {
                    rm_rf(&p)?;
                }
                continue;
            }
            if let (Some(stem), Some(ext)) = (p.file_stem(), p.extension()) {
                if ext != "json" && !self.has_layer(&stem.to_string_lossy()) && !dry_run {
                    rm_rf(&p)?;
                }
            }
        }
        let sizes = self.object_sizes()?;
        let meta_size = |k: &str| -> u64 {
            ["json", "dump", "erofs"].iter().map(|e| fs::symlink_metadata(self.layer_file(k, e)).map(|m| m.len()).unwrap_or(0)).sum()
        };
        let mut protected: HashSet<String> = HashSet::new();
        protected.insert(EMPTY_KEY.to_string());
        for keys in self.pins()?.values() {
            protected.extend(keys.iter().cloned());
        }
        for v in self.views()? {
            protected.insert(v.layer.clone());
        }
        let mut refs: HashMap<String, u32> = HashMap::new();
        let mut layer_objs: HashMap<String, Vec<String>> = HashMap::new();
        let mut lru: Vec<(SystemTime, String)> = Vec::new();
        let mut used: u64 = sizes.values().sum();
        for k in self.layers()? {
            let objs: Vec<String> = {
                let mut s: Vec<String> = self.dump(&k)?.iter().filter_map(|e| e.object().map(String::from)).collect();
                s.sort();
                s.dedup();
                s
            };
            for o in &objs {
                *refs.entry(o.clone()).or_default() += 1;
            }
            used += meta_size(&k);
            if !protected.contains(&k) {
                let m = fs::metadata(self.layer_file(&k, "json"))?.modified()?;
                lru.push((m, k.clone()));
            }
            layer_objs.insert(k, objs);
        }
        // Unreferenced objects are free to go first.
        let mut doomed_objs: Vec<String> = sizes.keys().filter(|o| !refs.contains_key(*o)).cloned().collect();
        let mut freed: u64 = doomed_objs.iter().map(|o| sizes[o]).sum();
        let before = used;
        let mut evicted = Vec::new();
        if used > high {
            lru.sort();
            for (_, k) in lru {
                if used - freed <= low {
                    break;
                }
                freed += meta_size(&k);
                for o in &layer_objs[&k] {
                    let c = refs.get_mut(o).unwrap();
                    *c -= 1;
                    if *c == 0 {
                        freed += sizes.get(o).copied().unwrap_or(0);
                        doomed_objs.push(o.clone());
                    }
                }
                evicted.push(k);
            }
        }
        if !dry_run {
            for k in &evicted {
                rm_rf(&self.layer_file(k, "json"))?;
                rm_rf(&self.layer_file(k, "erofs"))?;
                rm_rf(&self.layer_file(k, "dump"))?;
            }
            for o in &doomed_objs {
                rm_rf(&self.root.join("objects").join(o))?;
            }
        }
        Ok(serde_json::json!({
            "before_bytes": before, "after_bytes": before - freed, "freed_bytes": freed,
            "high": high, "low": low, "evicted_layers": evicted, "removed_objects": doomed_objs.len(),
            "protected_layers": protected.len(), "dry_run": dry_run, "secs": t.elapsed().as_secs_f64(),
        }))
    }

    /// Materialize a layer as a plain directory (source checkout on a builder).
    pub fn checkout(&self, key: &str, dst: &Path) -> Result<()> {
        fs::create_dir_all(dst)?;
        for e in self.dump(key)? {
            let p = dst.join(std::ffi::OsStr::from_bytes(&e.path[1..]));
            if e.path == b"/" {
                continue;
            }
            match e.mode & S_IFMT {
                S_IFDIR => fs::create_dir_all(&p)?,
                S_IFLNK => {
                    rm_rf(&p)?;
                    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(e.payload.as_deref().unwrap_or_default()), &p)?
                }
                S_IFREG if e.hardlink => {
                    rm_rf(&p)?;
                    fs::hard_link(dst.join(std::ffi::OsStr::from_bytes(&e.payload.as_deref().unwrap_or_default()[1..])), &p)?
                }
                S_IFREG => {
                    let tmp = p.with_extension("avo-tmp");
                    match (&e.content, e.object()) {
                        (Some(c), _) => fs::write(&tmp, c)?,
                        (None, Some(o)) => {
                            fs::copy(self.root.join("objects").join(o), &tmp)?;
                        }
                        _ => fs::write(&tmp, b"")?,
                    }
                    fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(e.mode & 0o7777))?;
                    fs::rename(&tmp, &p)?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

pub fn du(p: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    let mut seen = HashSet::new();
    while let Some(d) = stack.pop() {
        let Ok(md) = fs::symlink_metadata(&d) else { continue };
        if md.nlink() > 1 && !md.is_dir() && !seen.insert((md.dev(), md.ino())) {
            continue;
        }
        total += md.blocks() * 512;
        if md.is_dir() {
            if let Ok(rd) = fs::read_dir(&d) {
                stack.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()));
            }
        }
    }
    total
}

/// Read the whole of stdin and return when it closes (the build-lock holder's lifetime).
pub fn wait_stdin_eof() {
    let mut sink = Vec::new();
    let _ = std::io::stdin().lock().read_to_end(&mut sink);
}

pub fn read_lines(p: &Path) -> Result<Vec<String>> {
    Ok(BufReader::new(File::open(p)?).lines().collect::<std::io::Result<_>>()?)
}

pub fn write_out(v: &serde_json::Value) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_round_trips_escapes() {
        let e = Entry {
            path: b"/a dir/x=y\\-".to_vec(),
            size: 3,
            mode: S_IFREG | 0o644,
            hardlink: false,
            uid: 1,
            gid: 2,
            rdev: 0,
            mtime: (5, 7),
            payload: None,
            content: Some(b"a\nb".to_vec()),
            xattrs: vec![(b"user.k=1".to_vec(), b"v \x00".to_vec())],
        };
        let mut l = e.clone();
        l.path = b"/-".to_vec();
        l.hardlink = true;
        l.content = None;
        l.payload = Some(b"/a dir/x=y\\-".to_vec());
        let d = write_dump(&[e.clone(), l.clone()]);
        assert_eq!(parse_dump(&d).unwrap(), vec![e, l]);
    }
}
