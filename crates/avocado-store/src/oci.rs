//! OCI distribution protocol as transport only (decision 2.4).
//!
//! A layer `<key>` is pushed to `<registry>/<repo>` as an image index tagged `<key>`:
//!   manifests[0]  artifactType application/vnd.avocado.layer.v1
//!                 config = the layer record, layers = [erofs image, gzipped dump]
//!   manifests[1..] object chunks: up to CHUNK objects each, as plain blob descriptors.
//! The chunk manifests exist only so registry GC sees every object as referenced; a puller
//! reads the object list from the dump and fetches just the objects its store lacks.

use crate::{sha256_hex, Store};
use anyhow::{anyhow, bail, Context, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

const CHUNK: usize = 1000;
const MT_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const MT_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MT_LAYER: &str = "application/vnd.avocado.layer.v1";
const MT_RECORD: &str = "application/vnd.avocado.layer.record.v1+json";
const MT_EROFS: &str = "application/vnd.avocado.layer.erofs";
const MT_DUMP: &str = "application/vnd.avocado.layer.dump.v1+gzip";
const MT_OBJECT: &str = "application/vnd.avocado.object";
const MT_EMPTY: &str = "application/vnd.oci.empty.v1+json";

pub struct Remote {
    pub base: String,
    pub repo: String,
    agent: ureq::Agent,
}

#[derive(Default)]
pub struct Xfer {
    pub objects: AtomicU64,
    pub bytes: AtomicU64,
    pub skipped: AtomicU64,
    pub requests: AtomicU64,
}

impl Xfer {
    pub fn json(&self) -> Value {
        json!({
            "objects": self.objects.load(Ordering::Relaxed),
            "bytes": self.bytes.load(Ordering::Relaxed),
            "skipped_objects": self.skipped.load(Ordering::Relaxed),
            "requests": self.requests.load(Ordering::Relaxed),
        })
    }
}

fn threads() -> usize {
    16
}

impl Remote {
    /// `registry` is `http://host:port/repo/path`.
    pub fn parse(registry: &str) -> Result<Remote> {
        let rest = registry
            .strip_prefix("http://")
            .ok_or_else(|| anyhow!("registry must be http://host:port/repo (PoC: no TLS)"))?;
        let (host, repo) = rest.split_once('/').ok_or_else(|| anyhow!("registry needs a repo path"))?;
        Ok(Remote {
            base: format!("http://{host}"),
            repo: repo.trim_end_matches('/').to_string(),
            agent: ureq::AgentBuilder::new().max_idle_connections_per_host(64).build(),
        })
    }

    fn url(&self, tail: &str) -> String {
        format!("{}/v2/{}/{}", self.base, self.repo, tail)
    }

    pub fn has_blob(&self, digest: &str, x: &Xfer) -> Result<bool> {
        x.requests.fetch_add(1, Ordering::Relaxed);
        match self.agent.head(&self.url(&format!("blobs/sha256:{digest}"))).call() {
            Ok(_) => Ok(true),
            Err(ureq::Error::Status(404, _)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn put_blob(&self, digest: &str, data: &[u8], x: &Xfer) -> Result<()> {
        x.requests.fetch_add(2, Ordering::Relaxed);
        let r = self.agent.post(&self.url("blobs/uploads/")).call()?;
        let loc = r.header("Location").ok_or_else(|| anyhow!("upload without Location"))?.to_string();
        let loc = if loc.starts_with('/') { format!("{}{loc}", self.base) } else { loc };
        let sep = if loc.contains('?') { '&' } else { '?' };
        self.agent
            .put(&format!("{loc}{sep}digest=sha256:{digest}"))
            .set("Content-Type", "application/octet-stream")
            .send_bytes(data)?;
        x.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    pub fn get_blob(&self, digest: &str, x: &Xfer) -> Result<Vec<u8>> {
        x.requests.fetch_add(1, Ordering::Relaxed);
        let mut buf = Vec::new();
        self.agent
            .get(&self.url(&format!("blobs/sha256:{digest}")))
            .call()?
            .into_reader()
            .read_to_end(&mut buf)?;
        if sha256_hex(&buf) != digest {
            bail!("blob {digest}: digest mismatch");
        }
        x.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(buf)
    }

    fn put_manifest(&self, reference: &str, mt: &str, body: &[u8], x: &Xfer) -> Result<()> {
        x.requests.fetch_add(1, Ordering::Relaxed);
        self.agent
            .put(&self.url(&format!("manifests/{reference}")))
            .set("Content-Type", mt)
            .send_bytes(body)?;
        x.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    pub fn get_manifest(&self, reference: &str, x: &Xfer) -> Result<Option<Value>> {
        x.requests.fetch_add(1, Ordering::Relaxed);
        match self
            .agent
            .get(&self.url(&format!("manifests/{reference}")))
            .set("Accept", &format!("{MT_INDEX}, {MT_MANIFEST}"))
            .call()
        {
            Ok(r) => {
                let s = r.into_string()?;
                x.bytes.fetch_add(s.len() as u64, Ordering::Relaxed);
                Ok(Some(serde_json::from_str(&s)?))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Ensure a blob is present remotely (HEAD, then upload if missing).
    fn ensure_blob(&self, data: &[u8], x: &Xfer) -> Result<Value> {
        let d = sha256_hex(data);
        if !self.has_blob(&d, x)? {
            self.put_blob(&d, data, x)?;
        }
        Ok(json!({"digest": format!("sha256:{d}"), "size": data.len()}))
    }

    fn manifest_desc(body: &[u8], extra: Value) -> Value {
        let mut d = json!({"mediaType": MT_MANIFEST, "digest": format!("sha256:{}", sha256_hex(body)), "size": body.len()});
        if let (Some(o), Some(e)) = (d.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        d
    }
}

fn run_parallel<T: Sync>(items: &[T], f: impl Fn(&T) -> Result<()> + Sync) -> Result<()> {
    let next = AtomicUsize::new(0);
    let err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| loop {
                if err.lock().unwrap().is_some() {
                    break;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(it) = items.get(i) else { break };
                if let Err(e) = f(it) {
                    *err.lock().unwrap() = Some(e);
                    break;
                }
            });
        }
    });
    match err.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn objects_of(store: &Store, key: &str) -> Result<Vec<String>> {
    let mut v: Vec<String> = store.dump(key)?.iter().filter_map(|e| e.object().map(String::from)).collect();
    v.sort();
    v.dedup();
    Ok(v)
}

/// Push layer `key`. Objects already in a layer the remote has (`known` keys, typically the
/// parent) are not even probed; the rest are HEAD-checked and uploaded when missing.
pub fn push(store: &Store, remote: &Remote, key: &str, known: &[String]) -> Result<Value> {
    let t = Instant::now();
    let x = Xfer::default();
    let rec = store.record(key)?;
    let objs = objects_of(store, key)?;
    let mut skip: HashSet<String> = HashSet::new();
    for k in known {
        if store.has_layer(k) && remote.get_manifest(k, &x)?.is_some() {
            skip.extend(objects_of(store, k)?);
        }
    }
    let probe: Vec<&String> = objs.iter().filter(|o| !skip.contains(*o)).collect();
    x.skipped.fetch_add((objs.len() - probe.len()) as u64, Ordering::Relaxed);
    run_parallel(&probe, |o| {
        let d = o.replace('/', "");
        if !remote.has_blob(&d, &x)? {
            let data = fs::read(store.root.join("objects").join(o.as_str()))?;
            remote.put_blob(&d, &data, &x)?;
            x.objects.fetch_add(1, Ordering::Relaxed);
        } else {
            x.skipped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    })?;
    let record = serde_json::to_vec(&rec)?;
    let erofs = fs::read(store.erofs_path(key))?;
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&store.dump_bytes(key)?)?;
    let dump = gz.finish()?;
    let mut cfg = remote.ensure_blob(&record, &x)?;
    cfg["mediaType"] = json!(MT_RECORD);
    let mut l_erofs = remote.ensure_blob(&erofs, &x)?;
    l_erofs["mediaType"] = json!(MT_EROFS);
    let mut l_dump = remote.ensure_blob(&dump, &x)?;
    l_dump["mediaType"] = json!(MT_DUMP);
    let main = serde_json::to_vec(&json!({
        "schemaVersion": 2, "mediaType": MT_MANIFEST, "artifactType": MT_LAYER,
        "config": cfg, "layers": [l_erofs, l_dump],
        "annotations": {"org.avocado.layer.key": key, "org.avocado.layer.parent": rec.parent,
                        "org.avocado.layer.tree-digest": rec.tree_digest},
    }))?;
    let main_desc = Remote::manifest_desc(&main, json!({"artifactType": MT_LAYER}));
    remote.put_manifest(main_desc["digest"].as_str().unwrap(), MT_MANIFEST, &main, &x)?;
    let mut manifests = vec![main_desc];
    let empty = remote.ensure_blob(b"{}", &x)?;
    for chunk in objs.chunks(CHUNK) {
        let sizes: Vec<Value> = chunk
            .iter()
            .map(|o| {
                let size = fs::metadata(store.root.join("objects").join(o)).map(|m| m.len()).unwrap_or(0);
                json!({"mediaType": MT_OBJECT, "digest": format!("sha256:{}", o.replace('/', "")), "size": size})
            })
            .collect();
        let m = serde_json::to_vec(&json!({
            "schemaVersion": 2, "mediaType": MT_MANIFEST, "artifactType": "application/vnd.avocado.objects.v1",
            "config": {"mediaType": MT_EMPTY, "digest": empty["digest"], "size": 2}, "layers": sizes,
        }))?;
        let d = Remote::manifest_desc(&m, json!({"artifactType": "application/vnd.avocado.objects.v1"}));
        remote.put_manifest(d["digest"].as_str().unwrap(), MT_MANIFEST, &m, &x)?;
        manifests.push(d);
    }
    let index = serde_json::to_vec(&json!({
        "schemaVersion": 2, "mediaType": MT_INDEX, "artifactType": MT_LAYER, "manifests": manifests,
    }))?;
    remote.put_manifest(key, MT_INDEX, &index, &x)?;
    Ok(json!({"key": key, "op": "push", "layer_objects": objs.len(), "xfer": x.json(),
              "secs": t.elapsed().as_secs_f64()}))
}

/// Pull layer `key`: the image, the dump, and only the objects this store lacks.
pub fn pull(store: &Store, remote: &Remote, key: &str) -> Result<Value> {
    let t = Instant::now();
    let x = Xfer::default();
    let _gc = store.gc_lock(false)?;
    if store.has_layer(key) {
        return Ok(json!({"key": key, "op": "pull", "hit": true, "xfer": x.json(), "secs": t.elapsed().as_secs_f64()}));
    }
    let index = remote.get_manifest(key, &x)?.ok_or_else(|| anyhow!("{key}: not in {}/{}", remote.base, remote.repo))?;
    let main_d = index["manifests"][0]["digest"].as_str().ok_or_else(|| anyhow!("bad index"))?.to_string();
    let main = remote.get_manifest(&main_d, &x)?.ok_or_else(|| anyhow!("missing main manifest"))?;
    let digest = |v: &Value| v["digest"].as_str().unwrap_or("").trim_start_matches("sha256:").to_string();
    let record = remote.get_blob(&digest(&main["config"]), &x)?;
    let erofs = remote.get_blob(&digest(&main["layers"][0]), &x)?;
    let mut dump = Vec::new();
    GzDecoder::new(&remote.get_blob(&digest(&main["layers"][1]), &x)?[..]).read_to_end(&mut dump)?;
    let entries = crate::parse_dump(&dump)?;
    if sha256_hex(&dump) != serde_json::from_slice::<Value>(&record)?["tree_digest"].as_str().unwrap_or("") {
        bail!("{key}: dump does not match the record's tree digest");
    }
    let mut objs: Vec<String> = entries.iter().filter_map(|e| e.object().map(String::from)).collect();
    objs.sort();
    objs.dedup();
    let missing: Vec<&String> = objs.iter().filter(|o| !store.root.join("objects").join(o.as_str()).exists()).collect();
    x.skipped.fetch_add((objs.len() - missing.len()) as u64, Ordering::Relaxed);
    run_parallel(&missing, |o| {
        let d = o.replace('/', "");
        let data = remote.get_blob(&d, &x)?;
        let dst = store.obj_path(&d);
        fs::create_dir_all(dst.parent().unwrap())?;
        let tmp = store.root.join("tmp").join(format!("pull.{d}.{}", crate::uniq()));
        File::create(&tmp)?.write_all(&data)?;
        fs::rename(&tmp, &dst)?;
        x.objects.fetch_add(1, Ordering::Relaxed);
        Ok(())
    })?;
    let lf = |ext: &str| store.root.join("layers").join(format!("{key}.{ext}"));
    let pid = crate::uniq();
    fs::write(lf(&format!("dump.{pid}")), &dump)?;
    fs::write(lf(&format!("erofs.{pid}")), &erofs)?;
    fs::rename(lf(&format!("dump.{pid}")), lf("dump"))?;
    fs::rename(lf(&format!("erofs.{pid}")), lf("erofs"))?;
    fs::write(lf(&format!("json.{pid}")), &record).context("write record")?;
    fs::rename(lf(&format!("json.{pid}")), lf("json"))?;
    Ok(json!({"key": key, "op": "pull", "hit": false, "layer_objects": objs.len(), "xfer": x.json(),
              "secs": t.elapsed().as_secs_f64()}))
}
