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
                    let h = std::str::from_utf8(
                        s.get(i + 2..i + 4).ok_or_else(|| anyhow!("bad escape"))?,
                    )?;
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

/// A dump path must be rooted and canonical: `/`, or `/a/b` with no empty, `.` or `..`
/// component. Anything else could resolve outside the tree it describes.
pub fn valid_dump_path(p: &[u8]) -> bool {
    if p == b"/" {
        return true;
    }
    let Some(rest) = p.strip_prefix(b"/") else {
        return false;
    };
    rest.split(|&c| c == b'/')
        .all(|c| !c.is_empty() && c != b"." && c != b"..")
}

/// An object reference: `ab/` + 62 lowercase hex digits, the only form `resolve` writes.
pub fn valid_object_ref(p: &[u8]) -> bool {
    p.len() == 65
        && p[2] == b'/'
        && p.iter()
            .enumerate()
            .all(|(i, &c)| i == 2 || c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
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
            let pos = x
                .iter()
                .position(|&c| c == b'=')
                .ok_or_else(|| anyhow!("bad xattr"))?;
            xattrs.push((unesc(&x[..pos])?, unesc(&x[pos + 1..])?));
        }
        let path = unesc(f[0])?;
        if !valid_dump_path(&path) {
            bail!("invalid path in dump: {}", String::from_utf8_lossy(&path));
        }
        entries.push(Entry {
            path,
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
        let e = entries.last().unwrap();
        if e.xattrs.iter().any(|(k, _)| !keep_xattr(k)) {
            bail!(
                "dump entry {} carries an overlay or SELinux xattr",
                String::from_utf8_lossy(&e.path)
            );
        }
        match e.payload.as_deref() {
            Some(p) if e.hardlink && !valid_dump_path(p) => {
                bail!(
                    "invalid hardlink target in dump: {}",
                    String::from_utf8_lossy(p)
                )
            }
            Some(p) if e.is_reg() && !valid_object_ref(p) => {
                bail!(
                    "invalid object reference in dump: {}",
                    String::from_utf8_lossy(p)
                )
            }
            _ => {}
        }
    }
    Ok(entries)
}

/// A name unique across concurrent helpers: every helper is PID 1 in its own container, so a
/// pid says nothing; a random per-process tag plus a counter.
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
    let n =
        unsafe { libc::llistxattr(c.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOTSUP) {
            return Ok(Vec::new());
        }
        return Err(err).with_context(|| format!("llistxattr {}", p.display()));
    }
    let mut out = Vec::new();
    for name in buf[..n as usize]
        .split(|&c| c == 0)
        .filter(|n| !n.is_empty())
    {
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
        libc::lgetxattr(
            c.as_ptr(),
            cn.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        )
    };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if matches!(
            err.raw_os_error(),
            Some(libc::ENODATA) | Some(libc::ENOTSUP)
        ) {
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

/// What a walk asks of each regular file: reuse the parent's object, or the source to hash.
type Decide<'a> = dyn Fn(&[u8], &fs::Metadata, &Path) -> std::result::Result<Reuse, Src> + 'a;

/// Paths that never enter a layer, relative to the tree root (`includes`, `.stamps/ext`).
/// An entry ending in `/*` names every directory directly under its prefix
/// (`.stamps/rootfs/*`: the per-runtime image stamps, not the install stamp beside them).
#[derive(Clone, Debug, Default)]
pub struct Private {
    exact: HashSet<Vec<u8>>,
    dirs_under: Vec<Vec<u8>>,
}

impl Private {
    pub fn new(entries: &[String]) -> Private {
        let mut p = Private::default();
        for e in entries {
            let e = e.trim_matches('/');
            match e.strip_suffix("/*") {
                Some(prefix) => p.dirs_under.push(prefix.as_bytes().to_vec()),
                None => {
                    p.exact.insert(e.as_bytes().to_vec());
                }
            }
        }
        p
    }

    /// `rel` is the path without its leading `/`.
    pub fn matches(&self, rel: &[u8], is_dir: bool) -> bool {
        if self.exact.contains(rel) {
            return true;
        }
        is_dir
            && self.dirs_under.iter().any(|pre| {
                rel.len() > pre.len() + 1
                    && rel.starts_with(pre)
                    && rel[pre.len()] == b'/'
                    && !rel[pre.len() + 1..].contains(&b'/')
            })
    }

    /// Every private path present under `root`, relative to it.
    pub fn present_under(&self, root: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .exact
            .iter()
            .map(|e| PathBuf::from(std::ffi::OsStr::from_bytes(e)))
            .filter(|r| fs::symlink_metadata(root.join(r)).is_ok())
            .collect();
        for pre in &self.dirs_under {
            let base = PathBuf::from(std::ffi::OsStr::from_bytes(pre));
            if let Ok(rd) = fs::read_dir(root.join(&base)) {
                for d in rd.flatten() {
                    if d.file_type().is_ok_and(|t| t.is_dir()) {
                        out.push(base.join(d.file_name()));
                    }
                }
            }
        }
        out.sort();
        out
    }
}

struct Walk {
    entries: Vec<Entry>,
    todo: Vec<(usize, Src)>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Walk `root` depth-first with siblings sorted by name (the canonical order), skipping the
/// `private` paths. `decide` returns Ok(reuse) for regular files whose data is unchanged from
/// the parent layer, or the source to hash.
fn walk(root: &Path, private: &Private, decide: &Decide) -> Result<Walk> {
    let mut w = Walk {
        entries: Vec::new(),
        todo: Vec::new(),
    };
    let mut stack: Vec<(PathBuf, Vec<u8>)> = vec![(root.to_path_buf(), b"/".to_vec())];
    while let Some((full, path)) = stack.pop() {
        let md =
            fs::symlink_metadata(&full).with_context(|| format!("lstat {}", full.display()))?;
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
                let mut kids: Vec<_> = fs::read_dir(&full)
                    .with_context(|| format!("readdir {}", full.display()))?
                    .map(|d| d.map(|d| d.file_name()))
                    .collect::<std::io::Result<_>>()?;
                kids.sort();
                // Reverse so the stack pops them in sorted order.
                for k in kids.into_iter().rev() {
                    let mut kp = path.clone();
                    if kp != b"/" {
                        kp.push(b'/');
                    }
                    kp.extend_from_slice(k.as_bytes());
                    let kfull = full.join(&k);
                    let is_dir = fs::symlink_metadata(&kfull).is_ok_and(|m| m.is_dir());
                    if private.matches(&kp[1..], is_dir) {
                        continue;
                    }
                    stack.push((kfull, kp));
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
    /// Who the view belongs to, as the CLI recorded it (project dir, target, volume): what
    /// `report` shows and what the CLI's orphan check reads.
    #[serde(default)]
    pub meta: serde_json::Value,
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
        let op = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        if unsafe { libc::flock(f.as_raw_fd(), op) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Flock(f))
    }
}

pub fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output().with_context(|| format!("spawn {cmd:?}"))?;
    if !out.status.success() {
        bail!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn mount_erofs(image: &Path, at: &Path) -> Result<()> {
    let (src, dst) = (cstr(image), cstr(at));
    let fstype = CString::new("erofs").unwrap();
    let r = unsafe {
        libc::mount(
            src.as_ptr(),
            dst.as_ptr(),
            fstype.as_ptr(),
            libc::MS_RDONLY,
            std::ptr::null(),
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("mount erofs {}", image.display()));
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
        Ok(m) if m.is_dir() => {
            fs::remove_dir_all(p).with_context(|| format!("rm -r {}", p.display()))
        }
        Ok(_) => Ok(fs::remove_file(p)?),
        Err(_) => Ok(()),
    }
}

fn threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(32)
}

impl Store {
    pub fn open(root: &Path) -> Result<Store> {
        for d in ["objects", "layers", "views", "locks", "pins", "tmp"] {
            let p = root.join(d);
            fs::create_dir_all(&p)?;
            // Uppers hold project-private files and objects hold every project's files: only
            // root (the helper, and the kernel through the overlay) reads them directly.
            fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        }
        let s = Store {
            root: root.to_path_buf(),
        };
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
            s.write_layer(
                EMPTY_KEY,
                None,
                &[root_entry],
                serde_json::json!({}),
                (0, 0),
                0.0,
            )?;
        }
        Ok(s)
    }

    /// Flush the store's filesystem (one syncfs instead of an fsync per object).
    pub fn sync(&self) -> Result<()> {
        let f = File::open(&self.root)?;
        if unsafe { libc::syncfs(f.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("syncfs");
        }
        Ok(())
    }

    pub fn gc_lock(&self, exclusive: bool) -> Result<Flock> {
        Flock::take(&self.root.join("gc.lock"), exclusive)
    }

    pub fn key_lock(&self, key: &str) -> Result<Flock> {
        Flock::take(&self.root.join("locks").join(format!("{key}.lock")), true)
    }

    /// Serializes helpers that change one view (mount, use, commit, drop).
    pub fn view_lock(&self, name: &str) -> Result<Flock> {
        Flock::take(
            &self.root.join("locks").join(format!("view.{name}.lock")),
            true,
        )
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
        Ok(serde_json::from_slice(
            &fs::read(&p).with_context(|| format!("no layer {key}"))?,
        )?)
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
            .filter_map(|d| {
                d.file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".json"))
                    .map(String::from)
            })
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
        let tmp = self.root.join("tmp").join(format!(
            "obj.{hex}.{}.{}",
            crate::uniq(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
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
                        Ok((
                            Some(format!("{}/{}", &hex[..2], &hex[2..]).into_bytes()),
                            None,
                        ))
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
        run(Command::new("mkcomposefs")
            .arg("--from-file")
            .arg(&tmp_dump)
            .arg(&tmp_img))?;
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
        // The record is the commit point: everything it names (objects, dump, image) must be on
        // disk first, or a power loss leaves a record pointing at truncated files.
        self.sync()?;
        atomic_write(
            &self.layer_file(key, "json"),
            &serde_json::to_vec_pretty(&rec)?,
        )?;
        Ok(rec)
    }

    /// Import a directory tree as a complete layer.
    pub fn import_dir(
        &self,
        src: &Path,
        key: &str,
        inputs: serde_json::Value,
        exclude: &[String],
    ) -> Result<LayerRecord> {
        let _gc = self.gc_lock(false)?;
        if self.has_layer(key) {
            return self.record(key);
        }
        let t = Instant::now();
        let ex = Private::new(exclude);
        let mut w = walk(src, &ex, &|_, _, full| Err(Src::Copy(full.to_path_buf())))?;
        let stats = HashStats::default();
        self.resolve(&mut w, &stats)?;
        let new = (
            stats.new_objects.load(Ordering::Relaxed),
            stats.new_bytes.load(Ordering::Relaxed),
        );
        self.write_layer(
            key,
            None,
            &w.entries,
            inputs,
            new,
            t.elapsed().as_secs_f64(),
        )
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
        v.at.clone()
            .unwrap_or_else(|| self.view_dir(&v.name).join("merged"))
    }

    pub fn mount_view(
        &self,
        name: &str,
        layer: Option<&str>,
        private: &[String],
    ) -> Result<ViewRecord> {
        self.mount_view_at(name, layer, private, None, None)
    }

    pub fn mount_view_at(
        &self,
        name: &str,
        layer: Option<&str>,
        private: &[String],
        at: Option<&Path>,
        meta: Option<serde_json::Value>,
    ) -> Result<ViewRecord> {
        // Shared GC lock: GC clears tmp/ (where the staging mount lives) and must see the lease.
        let _gc = self.gc_lock(false)?;
        let mut v = self.view(name)?.unwrap_or(ViewRecord {
            name: name.to_string(),
            layer: EMPTY_KEY.to_string(),
            private: private.to_vec(),
            updated: now_secs(),
            at: None,
            meta: serde_json::Value::Null,
        });
        if !private.is_empty() {
            v.private = private.to_vec();
        }
        if let Some(a) = at {
            v.at = Some(a.to_path_buf());
        }
        if let Some(m) = meta {
            if v.meta != m {
                v.meta = m;
                self.save_view(&v)?;
            }
        }
        let dir = self.view_dir(name);
        let merged = self.merged(&v);
        if is_mounted(&merged) {
            // `use` and `commit` keep what the record lists: a private path the caller added
            // since the view was mounted must be recorded now, not at the next mount.
            if self.view(name)?.is_some_and(|r| r.private != v.private) {
                self.save_view(&v)?;
            }
            if let Some(l) = layer {
                if l != v.layer {
                    bail!(
                        "view {name} is mounted on {}, not {l}; use `use` to switch",
                        v.layer
                    );
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
                    let ts = [libc::timespec {
                        tv_sec: r.mtime.0,
                        tv_nsec: r.mtime.1 as _,
                    }; 2];
                    libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), 0);
                }
            }
        }
        // The lease is recorded before the mount, so GC never sees the mount without it.
        v.updated = now_secs();
        self.save_view(&v)?;
        self.touch(&v.layer);
        let tmp = self
            .root
            .join("tmp")
            .join(format!("erofs.{name}.{}", crate::uniq()));
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
        let r = run(Command::new("mount")
            .args(["-t", "overlay", "overlay", "-o", &opts])
            .arg(&merged));
        // The overlay holds the erofs superblock; the staging mount can go either way.
        let _ = run(Command::new("umount").arg("-l").arg(&tmp));
        let _ = fs::remove_dir(&tmp);
        r?;
        Ok(v)
    }

    pub fn umount_view(&self, name: &str) -> Result<()> {
        let Some(v) = self.view(name)? else {
            return Ok(());
        };
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
        let upper = dir.join("upper");
        for rel in Private::new(&v.private).present_under(&upper) {
            let to = next.join(&rel);
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(upper.join(&rel), to)?;
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
    pub fn commit_view(
        &self,
        name: &str,
        key: &str,
        inputs: serde_json::Value,
    ) -> Result<LayerRecord> {
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
        let pmap: HashMap<Vec<u8>, Entry> = self
            .dump(&parent)?
            .into_iter()
            .map(|e| (e.path.clone(), e))
            .collect();

        // What the upper holds: files with real data, metacopy files that were renamed (their
        // data comes from another lower path, so the parent's object for this path is wrong),
        // and redirected (renamed) dirs.
        let upper = dir.join("upper");
        let mut upper_data: HashSet<Vec<u8>> = HashSet::new();
        let mut renamed: HashSet<Vec<u8>> = HashSet::new();
        let mut redirected: Vec<Vec<u8>> = Vec::new();
        let uw = walk(&upper, &Private::default(), &|_, _, _| Ok((None, None)))?;
        for e in &uw.entries {
            let full = upper.join(std::ffi::OsStr::from_bytes(&e.path[1..]));
            let redirect = get_xattr(&full, b"trusted.overlay.redirect")?.is_some();
            match e.mode & S_IFMT {
                S_IFREG if get_xattr(&full, b"trusted.overlay.metacopy")?.is_none() => {
                    upper_data.insert(e.path.clone());
                }
                S_IFREG if redirect => {
                    renamed.insert(e.path.clone());
                }
                S_IFDIR if redirect => {
                    let mut p = e.path.clone();
                    p.push(b'/');
                    redirected.push(p);
                }
                _ => {}
            }
        }
        let ex = Private::new(&v.private);
        let decide =
            |path: &[u8], md: &fs::Metadata, full: &Path| -> std::result::Result<Reuse, Src> {
                if upper_data.contains(path) {
                    return Err(Src::Movable(
                        upper.join(std::ffi::OsStr::from_bytes(&path[1..])),
                    ));
                }
                if renamed.contains(path) {
                    return Err(Src::Copy(full.to_path_buf()));
                }
                if redirected.iter().any(|r| path.starts_with(r)) {
                    return Err(Src::Copy(full.to_path_buf()));
                }
                match pmap.get(path) {
                    Some(p) if p.is_reg() && p.size == md.size() => {
                        Ok((p.payload.clone(), p.content.clone()))
                    }
                    _ => Err(Src::Copy(full.to_path_buf())),
                }
            };
        let mut w = walk(&merged, &ex, &decide)?;
        // Files read through the view first; then unmount and move upper files into objects/.
        let (movable, through): (Vec<_>, Vec<_>) = std::mem::take(&mut w.todo)
            .into_iter()
            .partition(|(_, s)| matches!(s, Src::Movable(_)));
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
        let new = (
            stats.new_objects.load(Ordering::Relaxed),
            stats.new_bytes.load(Ordering::Relaxed),
        );
        let rec = match self.write_layer(
            key,
            Some(&parent),
            &w.entries,
            inputs,
            new,
            t.elapsed().as_secs_f64(),
        ) {
            Ok(rec) => rec,
            Err(e) => {
                // Upper files were already moved into objects/: the view cannot be put back as
                // it was. Return it to its parent (private paths kept); the step reruns.
                self.reset_upper(&v)?;
                self.mount_view(name, None, &[])?;
                return Err(e);
            }
        };
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
            let mut w = walk(&merged, &Private::default(), &|_, _, full| {
                Err(Src::Copy(full.to_path_buf()))
            })?;
            // Hash without storing: every object must already exist.
            let todo = std::mem::take(&mut w.todo);
            let next = AtomicUsize::new(0);
            let res: Mutex<Vec<(usize, Reuse)>> = Mutex::new(Vec::new());
            std::thread::scope(|s| {
                for _ in 0..threads() {
                    s.spawn(|| loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some((idx, Src::Copy(p))) = todo.get(i) else {
                            break;
                        };
                        let v = if w.entries[*idx].size <= INLINE_MAX {
                            (None, fs::read(p).ok())
                        } else {
                            let h = sha256_file(p).unwrap_or_default();
                            (
                                Some(
                                    format!(
                                        "{}/{}",
                                        &h[..2.min(h.len())],
                                        h.get(2..).unwrap_or("")
                                    )
                                    .into_bytes(),
                                ),
                                None,
                            )
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
        // Always point at the real layer, never at another alias: one hop to resolve, and
        // evicting the layer takes every name for it.
        let key = self.canonical(key);
        let key = key.as_str();
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
            let keys = fs::read_to_string(d.path())?
                .lines()
                .map(String::from)
                .collect();
            out.insert(d.file_name().to_string_lossy().to_string(), keys);
        }
        Ok(out)
    }

    pub fn pin(&self, name: &str, keys: &[String]) -> Result<()> {
        atomic_write(
            &self.root.join("pins").join(name),
            keys.join("\n").as_bytes(),
        )
    }

    pub fn unpin(&self, name: &str) -> Result<()> {
        rm_rf(&self.root.join("pins").join(name))
    }

    /// The layer `key` names: its alias target (followed through any chain), or itself.
    pub fn canonical(&self, key: &str) -> String {
        let mut k = key.to_string();
        for _ in 0..16 {
            match fs::read_link(self.layer_file(&k, "json"))
                .ok()
                .and_then(|t| {
                    t.to_str()
                        .and_then(|t| t.strip_suffix(".json"))
                        .map(String::from)
                }) {
                Some(next) if next != k => k = next,
                _ => break,
            }
        }
        k
    }

    fn is_alias(&self, key: &str) -> bool {
        fs::symlink_metadata(self.layer_file(key, "json")).is_ok_and(|m| m.file_type().is_symlink())
    }

    /// The canonical parents of `key`, nearest first.
    pub fn ancestors(&self, key: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut k = self.canonical(key);
        while let Ok(r) = self.record(&k) {
            let Some(p) = r.parent else { break };
            let c = self.canonical(&p);
            if c == k || out.contains(&c) {
                break;
            }
            out.push(c.clone());
            k = c;
        }
        out
    }

    /// Object → bytes on disk for every object.
    fn object_sizes(&self) -> Result<HashMap<String, u64>> {
        let mut out = HashMap::new();
        for d in fs::read_dir(self.root.join("objects"))? {
            let d = d?;
            let pre = d.file_name().to_string_lossy().to_string();
            for f in fs::read_dir(d.path())? {
                let f = f?;
                out.insert(
                    format!("{pre}/{}", f.file_name().to_string_lossy()),
                    disk_bytes(&f.metadata()?),
                );
            }
        }
        Ok(out)
    }

    /// Everything GC and `report` need, computed once, aliases folded into their targets.
    fn model(&self) -> Result<Model> {
        let mut m = Model {
            sizes: self.object_sizes()?,
            ..Model::default()
        };
        for k in self.layers()? {
            if self.is_alias(&k) {
                m.aliases.entry(self.canonical(&k)).or_default().push(k);
                continue;
            }
            let mut objs: Vec<String> = self
                .dump(&k)?
                .iter()
                .filter_map(|e| e.object().map(String::from))
                .collect();
            objs.sort();
            objs.dedup();
            for o in &objs {
                *m.refs.entry(o.clone()).or_default() += 1;
            }
            let meta: u64 = ["json", "dump", "erofs"]
                .iter()
                .map(|e| {
                    fs::symlink_metadata(self.layer_file(&k, e))
                        .map(|md| disk_bytes(&md))
                        .unwrap_or(0)
                })
                .sum();
            let rec = self.record(&k)?;
            m.parent
                .insert(k.clone(), rec.parent.as_deref().map(|p| self.canonical(p)));
            m.last_used.insert(
                k.clone(),
                fs::metadata(self.layer_file(&k, "json"))?.modified()?,
            );
            m.meta.insert(k.clone(), meta);
            m.objs.insert(k.clone(), objs);
            m.records.insert(k.clone(), rec);
            m.layers.push(k);
        }
        let mut protect = |k: &str, why: String| {
            let c = self.canonical(k);
            m.protected_by.entry(c).or_default().push(why);
        };
        protect(EMPTY_KEY, "base".to_string());
        for (name, keys) in self.pins()? {
            for k in keys {
                protect(&k, format!("pin {name}"));
            }
        }
        for v in self.views()? {
            protect(&v.layer, format!("view {}", v.name));
        }
        // A view also holds the layers its layer was built on: the next install recognizes them
        // as already in the view only while their records exist (and they share most of their
        // objects with the layer above, so keeping them costs little).
        let mut chain: Vec<(String, String)> = Vec::new();
        for v in self.views()? {
            let mut seen: HashSet<String> = HashSet::new();
            let mut k = self.canonical(&v.layer);
            while let Some(Some(p)) = m.parent.get(&k) {
                if !seen.insert(p.clone()) {
                    break;
                }
                chain.push((p.clone(), v.name.clone()));
                k = p.clone();
            }
        }
        for (k, view) in chain {
            m.protected_by
                .entry(k)
                .or_default()
                .push(format!("base of view {view}"));
        }
        Ok(m)
    }

    /// Where the store's space goes, for people: per layer what evicting it alone would free
    /// (`unique_bytes`), what holds it, and per view the private bytes it alone owns.
    pub fn report(&self) -> Result<serde_json::Value> {
        let m = self.model()?;
        let object_bytes: u64 = m.sizes.values().sum();
        let meta_bytes: u64 = m.meta.values().sum();
        let mut layers = Vec::new();
        for k in &m.layers {
            let r = &m.records[k];
            let unique: u64 = m.meta[k]
                + m.objs[k]
                    .iter()
                    .filter(|o| m.refs.get(*o) == Some(&1))
                    .map(|o| m.sizes.get(o).copied().unwrap_or(0))
                    .sum::<u64>();
            let children = m
                .parent
                .values()
                .filter(|p| p.as_deref() == Some(k.as_str()))
                .count();
            let last_used = m.last_used[k]
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            layers.push(serde_json::json!({
                "key": k, "label": r.inputs["label"], "parent": m.parent[k],
                "aliases": m.aliases.get(k).cloned().unwrap_or_default(),
                "created": r.created, "last_used": last_used, "files": r.files,
                "logical_bytes": r.logical_bytes, "unique_bytes": unique, "meta_bytes": m.meta[k],
                "children": children,
                "protected_by": m.protected_by.get(k).cloned().unwrap_or_default(),
            }));
        }
        let mut views = Vec::new();
        let mut upper_bytes = 0;
        for v in self.views()? {
            let upper = self.view_dir(&v.name).join("upper");
            let total = du(&upper);
            let private: u64 = Private::new(&v.private)
                .present_under(&upper)
                .iter()
                .map(|r| du(&upper.join(r)))
                .sum();
            upper_bytes += total;
            let layer = self.canonical(&v.layer);
            views.push(serde_json::json!({
                "name": v.name, "layer": layer, "mounted": is_mounted(&self.merged(&v)),
                "at": v.at, "meta": v.meta, "upper_bytes": total, "private_bytes": private,
                "layer_logical_bytes": m.records.get(&layer).map(|r| r.logical_bytes),
            }));
        }
        let reclaimable = self.plan_gc(&m, 0, 0).freed;
        Ok(serde_json::json!({
            "root": self.root, "free_bytes": free_bytes(&self.root),
            "total_bytes": object_bytes + meta_bytes + upper_bytes,
            "object_bytes": object_bytes, "objects": m.sizes.len(),
            "layer_meta_bytes": meta_bytes, "upper_bytes": upper_bytes,
            "reclaimable_bytes": reclaimable,
            "layers": layers, "views": views, "pins": self.pins()?,
        }))
    }

    /// What `gc(high, low)` would evict, without touching anything. Unreferenced objects go
    /// first; then, while over `low`, unprotected layers: leaves before the layers they were
    /// built on (a shared base frees little while its children exist, and every project needs
    /// it again), least recently used first among equals.
    fn plan_gc(&self, m: &Model, high: u64, low: u64) -> GcPlan {
        let mut refs = m.refs.clone();
        let mut doomed: Vec<String> = m
            .sizes
            .keys()
            .filter(|o| !refs.contains_key(*o))
            .cloned()
            .collect();
        let mut freed: u64 = doomed.iter().map(|o| m.sizes[o]).sum();
        let used: u64 = m.sizes.values().sum::<u64>() + m.meta.values().sum::<u64>();
        let mut evicted: Vec<(String, u64)> = Vec::new();
        if used > high {
            let mut remaining: HashSet<&String> = m.layers.iter().collect();
            loop {
                if used - freed <= low {
                    break;
                }
                let has_child =
                    |k: &String| remaining.iter().any(|c| m.parent[*c].as_ref() == Some(k));
                let candidates: Vec<&String> = remaining
                    .iter()
                    .copied()
                    .filter(|k| !m.protected_by.contains_key(*k))
                    .collect();
                let leaves: Vec<&String> = candidates
                    .iter()
                    .copied()
                    .filter(|k| !has_child(k))
                    .collect();
                let pool = if leaves.is_empty() {
                    &candidates
                } else {
                    &leaves
                };
                let Some(k) = pool
                    .iter()
                    .min_by_key(|k| (m.last_used[**k], (**k).clone()))
                    .map(|k| (*k).clone())
                else {
                    break;
                };
                let mut f = m.meta[&k];
                for o in &m.objs[&k] {
                    let c = refs.get_mut(o).unwrap();
                    *c -= 1;
                    if *c == 0 {
                        f += m.sizes.get(o).copied().unwrap_or(0);
                        doomed.push(o.clone());
                    }
                }
                freed += f;
                remaining.remove(&k);
                evicted.push((k, f));
            }
        }
        GcPlan {
            used,
            freed,
            evicted,
            doomed,
        }
    }

    /// Evict layers (see `plan_gc`) until the store is at or under `low` bytes, if it is over
    /// `high`, and drop objects no remaining layer references. Crash-safe: records go first (a
    /// layer without its json is garbage), objects last, all under the exclusive GC lock, so no
    /// writer can be mid-way referencing an object we drop.
    pub fn gc(&self, high: u64, low: u64, dry_run: bool) -> Result<serde_json::Value> {
        let t = Instant::now();
        let _gc = self.gc_lock(true)?;
        // Orphans from crashed writers: tmp files, dangling aliases, layer files without a record.
        if !dry_run {
            rm_rf(&self.root.join("tmp"))?;
            fs::create_dir_all(self.root.join("tmp"))?;
        }
        for d in fs::read_dir(self.root.join("layers"))? {
            let p = d?.path();
            if dry_run {
                continue;
            }
            if fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink())
                && fs::metadata(&p).is_err()
            {
                rm_rf(&p)?;
                continue;
            }
            if let (Some(stem), Some(ext)) = (p.file_stem(), p.extension()) {
                if ext != "json" && !self.has_layer(&stem.to_string_lossy()) {
                    rm_rf(&p)?;
                }
            }
        }
        let m = self.model()?;
        let plan = self.plan_gc(&m, high, low);
        if !dry_run {
            for (k, _) in &plan.evicted {
                for a in m.aliases.get(k).into_iter().flatten() {
                    for ext in ["json", "erofs", "dump"] {
                        rm_rf(&self.layer_file(a, ext))?;
                    }
                }
                for ext in ["json", "erofs", "dump"] {
                    rm_rf(&self.layer_file(k, ext))?;
                }
            }
            for o in &plan.doomed {
                rm_rf(&self.root.join("objects").join(o))?;
            }
        }
        Ok(serde_json::json!({
            "before_bytes": plan.used, "after_bytes": plan.used - plan.freed,
            "freed_bytes": plan.freed, "high": high, "low": low,
            "evicted_layers": plan.evicted.iter().map(|(k, f)| serde_json::json!({
                "key": k, "label": m.records[k].inputs["label"], "freed_bytes": f,
            })).collect::<Vec<_>>(),
            "removed_objects": plan.doomed.len(),
            "protected_layers": m.protected_by.len(), "dry_run": dry_run,
            "secs": t.elapsed().as_secs_f64(),
        }))
    }

    /// Drop every view whose name starts with `prefix` (a project's views, before its
    /// volume goes). Returns the names dropped.
    pub fn drop_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for v in self.views()? {
            if v.name.starts_with(prefix) {
                let _l = self.view_lock(&v.name)?;
                self.drop_view(&v.name)?;
                out.push(v.name);
            }
        }
        Ok(out)
    }
}

#[derive(Default)]
struct Model {
    /// object → bytes on disk
    sizes: HashMap<String, u64>,
    /// object → how many real layers reference it
    refs: HashMap<String, u32>,
    /// real layers (aliases folded in)
    layers: Vec<String>,
    /// layer → its alias names
    aliases: HashMap<String, Vec<String>>,
    objs: HashMap<String, Vec<String>>,
    /// layer → bytes of its json + dump + erofs
    meta: HashMap<String, u64>,
    parent: HashMap<String, Option<String>>,
    last_used: HashMap<String, SystemTime>,
    records: HashMap<String, LayerRecord>,
    /// layer → what keeps it from GC (`view <name>`, `pin <name>`, `base`)
    protected_by: HashMap<String, Vec<String>>,
}

struct GcPlan {
    used: u64,
    freed: u64,
    /// (layer, bytes evicting it freed), in eviction order
    evicted: Vec<(String, u64)>,
    doomed: Vec<String>,
}

/// Bytes a file takes on disk (what deleting it frees), not its length.
fn disk_bytes(md: &fs::Metadata) -> u64 {
    md.blocks() * 512
}

/// Bytes free to unprivileged users on the filesystem holding `p`.
pub fn free_bytes(p: &Path) -> u64 {
    let c = cstr(p);
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return 0;
    }
    s.f_bavail as u64 * s.f_frsize as u64
}

pub fn du(p: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    let mut seen = HashSet::new();
    while let Some(d) = stack.pop() {
        let Ok(md) = fs::symlink_metadata(&d) else {
            continue;
        };
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
    Ok(BufReader::new(File::open(p)?)
        .lines()
        .collect::<std::io::Result<_>>()?)
}

pub fn write_out(v: &serde_json::Value) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GC evicts unprotected layers and their objects, and a pinned *alias* keeps its target.
    /// Needs `mkcomposefs` on PATH (no root: GC never mounts).
    #[test]
    fn gc_keeps_alias_targets_and_drops_the_rest() {
        let dir = std::env::temp_dir().join(format!("avo-store-test-{}", uniq()));
        let st = Store::open(&dir).unwrap();
        let file = |name: &str, body: &[u8]| {
            let p = dir.join(name);
            fs::write(&p, body).unwrap();
            let hex = sha256_hex(body);
            st.put_object(&p, &hex, false).unwrap();
            Entry {
                path: format!("/{name}").into_bytes(),
                size: body.len() as u64,
                mode: S_IFREG | 0o644,
                hardlink: false,
                uid: 0,
                gid: 0,
                rdev: 0,
                mtime: (0, 0),
                payload: Some(format!("{}/{}", &hex[..2], &hex[2..]).into_bytes()),
                content: None,
                xattrs: vec![],
            }
        };
        let root = st.dump(EMPTY_KEY).unwrap().remove(0);
        let a = file("a", &[b'a'; 100]);
        let b = file("b", &[b'b'; 100]);
        st.write_layer(
            "kept",
            None,
            &[root.clone(), a.clone()],
            serde_json::json!({}),
            (1, 100),
            0.0,
        )
        .unwrap();
        st.write_layer(
            "dropped",
            None,
            &[root, b.clone()],
            serde_json::json!({}),
            (1, 100),
            0.0,
        )
        .unwrap();
        st.alias("kept-alias", "kept").unwrap();
        st.pin("p", &["kept-alias".to_string()]).unwrap();
        let r = st.gc(0, 0, false).unwrap();
        assert!(st.has_layer("kept") && st.has_layer("kept-alias"), "{r}");
        assert!(!st.has_layer("dropped"), "{r}");
        let obj = |e: &Entry| {
            dir.join("objects")
                .join(std::str::from_utf8(e.payload.as_ref().unwrap()).unwrap())
        };
        assert!(obj(&a).exists(), "the pinned alias's object must survive");
        assert!(!obj(&b).exists(), "the evicted layer's object must go");
        let _ = fs::remove_dir_all(&dir);
    }

    fn test_store() -> (PathBuf, Store) {
        let dir = std::env::temp_dir().join(format!("avo-store-test-{}", uniq()));
        let st = Store::open(&dir).unwrap();
        (dir, st)
    }

    /// A regular-file entry backed by a real object holding `body`.
    fn obj_entry(st: &Store, dir: &Path, name: &str, body: &[u8]) -> Entry {
        let p = dir.join(format!("src-{name}"));
        fs::write(&p, body).unwrap();
        let hex = sha256_hex(body);
        st.put_object(&p, &hex, false).unwrap();
        Entry {
            path: format!("/{name}").into_bytes(),
            size: body.len() as u64,
            mode: S_IFREG | 0o644,
            hardlink: false,
            uid: 0,
            gid: 0,
            rdev: 0,
            mtime: (0, 0),
            payload: Some(format!("{}/{}", &hex[..2], &hex[2..]).into_bytes()),
            content: None,
            xattrs: vec![],
        }
    }

    fn layer(st: &Store, key: &str, parent: Option<&str>, files: &[Entry]) {
        let mut entries = vec![st.dump(EMPTY_KEY).unwrap().remove(0)];
        entries.extend(files.iter().cloned());
        st.write_layer(
            key,
            parent,
            &entries,
            serde_json::json!({"label": key}),
            (0, 0),
            0.0,
        )
        .unwrap();
    }

    fn age(st: &Store, key: &str, secs_ago: i64) {
        let p = cstr(&st.layer_file(key, "json"));
        let t = now_secs() as i64 - secs_ago;
        let ts = [libc::timespec {
            tv_sec: t,
            tv_nsec: 0,
        }; 2];
        unsafe { libc::utimensat(libc::AT_FDCWD, p.as_ptr(), ts.as_ptr(), 0) };
    }

    /// GC evicts a layer before the base it was built on, even when the base is older: the
    /// base is what every project needs again.
    #[test]
    fn gc_evicts_leaves_before_the_base_they_were_built_on() {
        let (dir, st) = test_store();
        let shared = obj_entry(&st, &dir, "shared", &[b's'; 4096]);
        let top = obj_entry(&st, &dir, "top", &[b't'; 4096]);
        layer(&st, "base", None, std::slice::from_ref(&shared));
        layer(&st, "child", Some("base"), &[shared, top]);
        age(&st, "base", 1000);
        age(&st, "child", 10);
        let before = st.gc(u64::MAX, 0, true).unwrap()["before_bytes"]
            .as_u64()
            .unwrap();
        // Ask for just under the current size: one eviction is enough.
        let r = st.gc(0, before - 1, false).unwrap();
        let ev: Vec<&str> = r["evicted_layers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["key"].as_str().unwrap())
            .collect();
        assert_eq!(ev, vec!["child"], "{r}");
        assert!(st.has_layer("base"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// An alias of an alias resolves to the real layer, so a step that looks a layer up by
    /// either name finds it in the view's chain.
    #[test]
    fn alias_chains_resolve_to_the_real_layer() {
        let (dir, st) = test_store();
        let a = obj_entry(&st, &dir, "a", &[b'a'; 4096]);
        layer(&st, "real", None, &[a]);
        st.alias("one", "real").unwrap();
        st.alias("two", "one").unwrap();
        assert_eq!(st.canonical("two"), "real");
        assert_eq!(
            fs::read_link(st.layer_file("two", "json")).unwrap(),
            PathBuf::from("real.json"),
            "aliases point at the real layer, never at another alias"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A view holds its layer and every layer under it: evicting a base would make the next
    /// install rebuild what the view already contains.
    #[test]
    fn gc_keeps_the_layers_a_view_was_built_on() {
        let (dir, st) = test_store();
        let a = obj_entry(&st, &dir, "a", &[b'a'; 4096]);
        let b = obj_entry(&st, &dir, "b", &[b'b'; 4096]);
        layer(&st, "l0", None, std::slice::from_ref(&a));
        layer(&st, "l1", Some("l0"), &[a.clone(), b]);
        layer(&st, "other", None, &[a]);
        st.save_view(&ViewRecord {
            name: "v".into(),
            layer: "l1".into(),
            ..ViewRecord::default()
        })
        .unwrap();
        st.save_view(&ViewRecord {
            name: "w".into(),
            layer: "l1".into(),
            ..ViewRecord::default()
        })
        .unwrap();
        let r = st.gc(0, 0, false).unwrap();
        assert!(st.has_layer("l0") && st.has_layer("l1"), "{r}");
        assert!(!st.has_layer("other"), "{r}");
        let rep = st.report().unwrap();
        let l0 = rep["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["key"] == "l0")
            .unwrap()
            .clone();
        let mut by: Vec<String> = serde_json::from_value(l0["protected_by"].clone()).unwrap();
        by.sort();
        assert_eq!(by, vec!["base of view v", "base of view w"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// An alias is not a second layer: it is not counted, not evicted on its own, and goes
    /// with its target.
    #[test]
    fn gc_folds_aliases_into_their_target() {
        let (dir, st) = test_store();
        let a = obj_entry(&st, &dir, "a", &[b'a'; 4096]);
        layer(&st, "real", None, &[a]);
        st.alias("other-name", "real").unwrap();
        let rep = st.report().unwrap();
        let keys: Vec<&str> = rep["layers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["key"].as_str().unwrap())
            .collect();
        assert!(
            keys.contains(&"real") && !keys.contains(&"other-name"),
            "{rep}"
        );
        let real = rep["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["key"] == "real")
            .unwrap();
        assert_eq!(real["aliases"], serde_json::json!(["other-name"]));
        let r = st.gc(0, 0, false).unwrap();
        assert_eq!(r["evicted_layers"].as_array().unwrap().len(), 1, "{r}");
        assert!(!st.has_layer("real") && !st.has_layer("other-name"));
        assert!(fs::symlink_metadata(st.layer_file("other-name", "json")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// `unique_bytes` is what evicting that layer alone frees: objects shared with another
    /// layer don't count.
    #[test]
    fn report_unique_bytes_excludes_shared_objects() {
        let (dir, st) = test_store();
        let shared = obj_entry(&st, &dir, "shared", &[b's'; 8192]);
        let mine = obj_entry(&st, &dir, "mine", &[b'm'; 4096]);
        layer(&st, "one", None, &[shared.clone(), mine]);
        layer(&st, "two", None, &[shared]);
        st.pin("keep", &["two".to_string()]).unwrap();
        let rep = st.report().unwrap();
        let get = |k: &str| {
            rep["layers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["key"] == k)
                .unwrap()
                .clone()
        };
        let one = get("one");
        let meta = one["meta_bytes"].as_u64().unwrap();
        assert_eq!(one["unique_bytes"].as_u64().unwrap(), meta + 4096, "{one}");
        assert_eq!(get("two")["protected_by"], serde_json::json!(["pin keep"]));
        // Only "one" is reclaimable; evicting it frees exactly its unique bytes.
        assert_eq!(rep["reclaimable_bytes"], one["unique_bytes"], "{rep}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dump_paths_must_be_rooted_and_canonical() {
        for good in [&b"/"[..], b"/a", b"/a/b.c", b"/.stamps/x"] {
            assert!(valid_dump_path(good), "{}", String::from_utf8_lossy(good));
        }
        for bad in [
            &b""[..],
            b"a",
            b"//a",
            b"/a/",
            b"/a/../b",
            b"/./a",
            b"/..",
            b"/a//b",
        ] {
            assert!(!valid_dump_path(bad), "{}", String::from_utf8_lossy(bad));
        }
        let ok = format!("ab/{}", "c".repeat(62));
        assert!(valid_object_ref(ok.as_bytes()));
        for bad in [
            "../etc/shadow",
            "/abs/path",
            &format!("AB/{}", "c".repeat(62)),
            "ab/cd",
        ] {
            assert!(!valid_object_ref(bad.as_bytes()), "{bad}");
        }
    }

    /// A dump that would make checkout or mount reach outside its tree is refused.
    #[test]
    fn parse_dump_refuses_escaping_entries() {
        let line = |path: &str, mode: &str, payload: &str, xattr: &str| {
            format!("{path} 100 {mode} 1 0 0 0 0.0 {payload} - -{xattr}\n")
        };
        let obj = format!("ab/{}", "c".repeat(62));
        let root = line("/", "40755", "-", "");
        assert!(parse_dump((root.clone() + &line("/f", "100644", &obj, "")).as_bytes()).is_ok());
        for bad in [
            line("/../views/x/upper/planted", "100644", &obj, ""),
            line("relative", "100644", &obj, ""),
            line("/f", "100644", "../../views/x/upper/secret", ""),
            line("/f", "@100644", "../outside", ""),
            line("/f", "100644", &obj, " trusted.overlay.redirect=/etc"),
        ] {
            assert!(
                parse_dump((root.clone() + &bad).as_bytes()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn private_paths_match_nested_and_per_directory_entries() {
        let p = Private::new(&[
            "includes".to_string(),
            ".stamps/ext".to_string(),
            ".stamps/rootfs/*".to_string(),
        ]);
        assert!(p.matches(b"includes", true));
        assert!(p.matches(b".stamps/ext", true));
        assert!(!p.matches(b".stamps", true));
        assert!(!p.matches(b".stamps/sdk", true));
        // Every runtime's image-stamp dir, but not the install stamp beside them.
        assert!(p.matches(b".stamps/rootfs/dev", true));
        assert!(!p.matches(b".stamps/rootfs/install.stamp", false));
        assert!(!p.matches(b".stamps/rootfs/dev/image.stamp", false));
        assert!(!p.matches(b"sdk/includes", true));
    }

    /// A walk (what commit harvests) leaves private paths out at any depth, and keeps the
    /// shared files beside them.
    #[test]
    fn walk_skips_nested_private_paths() {
        let dir = std::env::temp_dir().join(format!("avo-walk-{}", uniq()));
        for d in [
            ".stamps/ext/app",
            ".stamps/rootfs/dev",
            ".stamps/sdk/x86_64",
            "includes",
        ] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        for f in [
            ".stamps/ext/app/build.stamp",
            ".stamps/rootfs/dev/image.stamp",
            ".stamps/rootfs/install.stamp",
            ".stamps/sdk/x86_64/install.stamp",
            "includes/x",
        ] {
            fs::write(dir.join(f), b"1").unwrap();
        }
        let p = Private::new(&[
            "includes".into(),
            ".stamps/ext".into(),
            ".stamps/rootfs/*".into(),
        ]);
        let w = walk(&dir, &p, &|_, _, _| Ok((None, None))).unwrap();
        let paths: Vec<String> = w
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect();
        assert!(
            paths.contains(&"/.stamps/rootfs/install.stamp".to_string()),
            "{paths:?}"
        );
        assert!(paths.contains(&"/.stamps/sdk/x86_64/install.stamp".to_string()));
        for gone in ["/includes", "/.stamps/ext", "/.stamps/rootfs/dev"] {
            assert!(
                !paths.iter().any(|p| p.starts_with(gone)),
                "{gone} in {paths:?}"
            );
        }
        let present = p.present_under(&dir);
        assert_eq!(
            present,
            vec![
                PathBuf::from(".stamps/ext"),
                PathBuf::from(".stamps/rootfs/dev"),
                PathBuf::from("includes"),
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Integration tests: real mounts. They need root, `mkcomposefs`, kernel 6.12+ and a store
    /// directory on a filesystem that can hold an overlay upper (not overlayfs or tmpfs inside
    /// a container): `tests/integration.sh` runs them in the helper image with
    /// AVOCADO_STORE_IT_DIR set. Without it they are skipped.
    fn it_store() -> Option<(PathBuf, Store)> {
        let base = std::env::var("AVOCADO_STORE_IT_DIR").ok()?;
        let dir = Path::new(&base).join(format!("it-{}", uniq()));
        let st = Store::open(&dir).unwrap();
        Some((dir, st))
    }

    fn it_cleanup(dir: &Path, st: &Store) {
        for v in st.views().unwrap() {
            let _ = st.drop_view(&v.name);
        }
        let _ = fs::remove_dir_all(dir);
    }

    fn file_object(st: &Store, key: &str, path: &str) -> Option<String> {
        st.dump(key)
            .unwrap()
            .into_iter()
            .find(|e| e.path == path.as_bytes())
            .and_then(|e| e.object().map(String::from))
    }

    #[test]
    #[ignore = "needs root and real mounts: tests/integration.sh"]
    fn it_commit_use_round_trip_and_verify() {
        let Some((dir, st)) = it_store() else { return };
        let priv_ = vec!["includes".to_string(), ".stamps/ext".to_string()];
        let v = st.mount_view("v", Some(EMPTY_KEY), &priv_).unwrap();
        let m = st.merged(&v);
        fs::create_dir_all(m.join("sdk/bin")).unwrap();
        fs::write(m.join("sdk/bin/tool"), vec![b'x'; 10_000]).unwrap();
        fs::create_dir_all(m.join("includes")).unwrap();
        fs::write(m.join("includes/secret"), b"project only").unwrap();
        fs::create_dir_all(m.join(".stamps/ext/app")).unwrap();
        fs::write(m.join(".stamps/ext/app/build.stamp"), b"1").unwrap();
        let rec = st.commit_view("v", "k1", serde_json::json!({})).unwrap();
        assert_eq!(rec.parent.as_deref(), Some(EMPTY_KEY));
        let paths: Vec<String> = st
            .dump("k1")
            .unwrap()
            .iter()
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect();
        assert!(paths.contains(&"/sdk/bin/tool".to_string()), "{paths:?}");
        assert!(!paths
            .iter()
            .any(|p| p.starts_with("/includes") || p.starts_with("/.stamps/ext")));
        // The view is on k1 with its private files still there.
        assert_eq!(
            fs::read(m.join("includes/secret")).unwrap(),
            b"project only"
        );
        assert!(m.join(".stamps/ext/app/build.stamp").exists());
        // Switching layers keeps private paths and drops shared-path edits.
        fs::write(m.join("sdk/bin/scratch"), b"gone after use").unwrap();
        st.use_layer("v", EMPTY_KEY, &[]).unwrap();
        assert!(!m.join("sdk").exists());
        assert!(m.join("includes/secret").exists() && m.join(".stamps/ext/app").exists());
        st.use_layer("v", "k1", &[]).unwrap();
        assert!(!m.join("sdk/bin/scratch").exists());
        assert_eq!(st.ancestors("k1"), vec![EMPTY_KEY.to_string()]);
        let (ok, _, _) = st.verify("k1").unwrap();
        assert!(ok, "verify digest");
        it_cleanup(&dir, &st);
    }

    /// Swapping two equal-size files is a metacopy rename; the committed layer must follow
    /// the view, not the parent's object for each name.
    #[test]
    #[ignore = "needs root and real mounts: tests/integration.sh"]
    fn it_commit_after_an_equal_size_swap_records_the_view() {
        let Some((dir, st)) = it_store() else { return };
        let v = st.mount_view("v", Some(EMPTY_KEY), &[]).unwrap();
        let m = st.merged(&v);
        fs::write(m.join("a"), vec![b'A'; 4096]).unwrap();
        fs::write(m.join("b"), vec![b'B'; 4096]).unwrap();
        st.commit_view("v", "base", serde_json::json!({})).unwrap();
        fs::rename(m.join("a"), m.join("t")).unwrap();
        fs::rename(m.join("b"), m.join("a")).unwrap();
        fs::rename(m.join("t"), m.join("b")).unwrap();
        assert_eq!(fs::read(m.join("a")).unwrap(), vec![b'B'; 4096]);
        st.commit_view("v", "swapped", serde_json::json!({}))
            .unwrap();
        let hex = |b: u8| {
            let h = sha256_hex(&[b; 4096]);
            format!("{}/{}", &h[..2], &h[2..])
        };
        assert_eq!(file_object(&st, "swapped", "/a"), Some(hex(b'B')));
        assert_eq!(file_object(&st, "swapped", "/b"), Some(hex(b'A')));
        assert_eq!(fs::read(m.join("a")).unwrap(), vec![b'B'; 4096]);
        it_cleanup(&dir, &st);
    }

    /// A commit that fails after the upper was harvested puts the view back on its parent,
    /// mounted, with private paths kept, so the step can simply rerun.
    #[test]
    #[ignore = "needs root and real mounts: tests/integration.sh"]
    fn it_failed_commit_leaves_the_view_on_its_parent() {
        let Some((dir, st)) = it_store() else { return };
        let v = st
            .mount_view("v", Some(EMPTY_KEY), &["includes".to_string()])
            .unwrap();
        let m = st.merged(&v);
        fs::write(m.join("big"), vec![b'z'; 100_000]).unwrap();
        fs::create_dir_all(m.join("includes")).unwrap();
        fs::write(m.join("includes/keep"), b"k").unwrap();
        // mkcomposefs that always fails, first on PATH.
        let shim = dir.join("shim");
        fs::create_dir_all(&shim).unwrap();
        fs::write(shim.join("mkcomposefs"), "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(
            shim.join("mkcomposefs"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", shim.display()));
        let r = st.commit_view("v", "never", serde_json::json!({}));
        std::env::set_var("PATH", path);
        assert!(r.is_err());
        assert!(!st.has_layer("never"));
        assert!(is_mounted(&m), "the view must be mounted again");
        assert_eq!(st.view("v").unwrap().unwrap().layer, EMPTY_KEY);
        assert!(!m.join("big").exists());
        assert_eq!(fs::read(m.join("includes/keep")).unwrap(), b"k");
        it_cleanup(&dir, &st);
    }

    /// A view with a file open cannot be dropped out from under its user: the drop fails and
    /// the view is still recorded and mounted.
    #[test]
    #[ignore = "needs root and real mounts: tests/integration.sh"]
    fn it_drop_refuses_a_busy_view() {
        let Some((dir, st)) = it_store() else { return };
        let v = st.mount_view("v", Some(EMPTY_KEY), &[]).unwrap();
        let m = st.merged(&v);
        fs::write(m.join("f"), b"x").unwrap();
        let held = File::open(m.join("f")).unwrap();
        assert!(st.drop_view("v").is_err());
        assert!(st.view("v").unwrap().is_some() && is_mounted(&m));
        drop(held);
        st.drop_view("v").unwrap();
        assert!(st.view("v").unwrap().is_none());
        it_cleanup(&dir, &st);
    }

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
