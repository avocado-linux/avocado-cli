//! Route planner (PoC): where should a project build, and how do its bytes move?
//!
//! Inputs: backend descriptors (what each machine advertises: capacity, warm layers, the
//! Rubicons whose hub it is or shares a store with), links between machines (measured or
//! declared bandwidth and kind: lan, site-vpn, ssh hop, connect relay), the client the source
//! lives on, the board, and the project's sizes. For every backend that can build, it costs:
//!   source   client → builder (content-hashed sync: only what the builder lacks)
//!   layers   SDK/sysroot layers the builder lacks, from the cheapest warm store (or the feed)
//!   build    cpu-seconds / cores
//!   bundle   builder → the board's hub; 0 when the builder's store *is* the hub's store
//! and picks the fastest, bytes as the tie-break.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

/// Best (seconds, bytes-path) to move `bytes` from `a` to `b` over `links`, at most one relay.
fn transfer(links: &[Value], a: &str, b: &str, bytes: f64) -> (f64, String) {
    if a == b || bytes == 0.0 {
        return (0.0, "local".into());
    }
    let mbps = |x: &str, y: &str| -> Option<(f64, String)> {
        links.iter().find_map(|l| {
            let (la, lb) = (l["a"].as_str()?, l["b"].as_str()?);
            ((la == x && lb == y) || (la == y && lb == x))
                .then(|| (f(&l["mbps"]), l["kind"].as_str().unwrap_or("link").to_string()))
        })
    };
    let secs = |m: f64| bytes * 8.0 / (m * 1e6);
    let mut best = (f64::INFINITY, "unreachable".to_string());
    if let Some((m, k)) = mbps(a, b) {
        best = (secs(m), format!("{a}→{b} ({k}, {m:.0} Mbit/s)"));
    }
    let nodes: Vec<&str> = links.iter().flat_map(|l| [l["a"].as_str(), l["b"].as_str()]).flatten().collect();
    for via in nodes {
        if via == a || via == b {
            continue;
        }
        if let (Some((m1, k1)), Some((m2, k2))) = (mbps(a, via), mbps(via, b)) {
            let t = secs(m1.min(m2));
            if t < best.0 {
                best = (t, format!("{a}→{via}→{b} ({k1}+{k2}, {:.0} Mbit/s)", m1.min(m2)));
            }
        }
    }
    best
}

fn warm(backend: &Value, target: &str, releasever: Option<&str>) -> bool {
    backend["store"]["layers"].as_array().is_some_and(|ls| {
        ls.iter().any(|l| {
            let i = &l["inputs"]["inputs"];
            i["target"].as_str() == Some(target)
                && releasever.is_none_or(|r| i["releasever"].as_str() == Some(r))
        })
    })
}

pub fn plan(topo: &Value) -> Result<Value> {
    let p = &topo["project"];
    let target = p["target"].as_str().ok_or_else(|| anyhow!("project.target"))?;
    let rv = p["releasever"].as_str();
    let client = topo["client"].as_str().ok_or_else(|| anyhow!("client"))?;
    let board = topo["board"].as_str().ok_or_else(|| anyhow!("board"))?;
    let links = topo["links"].as_array().cloned().unwrap_or_default();
    let backends = topo["backends"].as_array().cloned().unwrap_or_default();
    let by_name: HashMap<&str, &Value> = backends.iter().filter_map(|b| Some((b["name"].as_str()?, b))).collect();
    let hub = backends
        .iter()
        .find(|b| b["rubicons"].as_array().is_some_and(|r| r.iter().any(|x| x == board)))
        .ok_or_else(|| anyhow!("no backend is the hub for {board}"))?;
    let hub_name = hub["name"].as_str().unwrap_or("");
    let feed = topo["feed"].clone();
    let mut rows = Vec::new();
    for b in backends.iter().filter(|b| b["can_build"].as_bool().unwrap_or(false)) {
        let name = b["name"].as_str().unwrap_or("");
        let (t_src, path_src) = transfer(&links, client, name, f(&p["source_bytes"]));
        // Layers: none if warm here; else from the cheapest warm peer; else the package feed.
        let (sdk_b, t_sdk, path_sdk) = if warm(b, target, rv) {
            (0.0, 0.0, "warm".to_string())
        } else {
            let bytes = f(&p["layer_bytes"]);
            // The feed is reached from the builder itself, at the builder's own internet speed.
            let fm = if b["feed_mbps"].is_number() { f(&b["feed_mbps"]) } else { f(&feed["mbps"]) };
            let mut best = if b["offline"].as_bool().unwrap_or(false) || fm == 0.0 {
                (bytes, f64::INFINITY, "no feed reachable".to_string())
            } else {
                (bytes, bytes * 8.0 / (fm * 1e6) + f(&feed["install_s"]), format!("package feed ({fm:.0} Mbit/s) + install"))
            };
            for peer in backends.iter().filter(|x| x["name"] != b["name"] && warm(x, target, rv)) {
                let (t, path) = transfer(&links, peer["name"].as_str().unwrap_or(""), name, bytes);
                if t < best.1 {
                    best = (bytes, t, format!("pull from {}: {path}", peer["name"].as_str().unwrap_or("")));
                }
            }
            best
        };
        let build_s = f(&p["build_cpu_s"]) / f(&b["cpus"]).max(1.0);
        // Bundle to the hub: zero when the builder's store is the hub's store (same machine).
        let same_store = name == hub_name || b["store_shared_with"].as_array().is_some_and(|s| s.iter().any(|x| x == hub_name));
        let (bundle_b, t_bundle, path_bundle) = if same_store {
            (0.0, 0.0, "same store as the hub: zero-copy".to_string())
        } else {
            let bytes = f(&p["bundle_bytes"]);
            let (t, path) = transfer(&links, name, hub_name, bytes);
            (bytes, t, path)
        };
        let total = t_src + t_sdk + build_s + t_bundle;
        rows.push(json!({
            "builder": name, "est_total_s": total,
            "source": {"bytes": p["source_bytes"], "s": t_src, "path": path_src},
            "layers": {"bytes": sdk_b, "s": t_sdk, "path": path_sdk},
            "build_s": build_s,
            "bundle": {"bytes": bundle_b, "s": t_bundle, "path": path_bundle},
            "bytes_after_build": bundle_b,
            "bytes_total": f(&p["source_bytes"]) + sdk_b + bundle_b,
        }));
    }
    rows.sort_by(|a, b| {
        f(&a["est_total_s"]).total_cmp(&f(&b["est_total_s"])).then(f(&a["bytes_total"]).total_cmp(&f(&b["bytes_total"])))
    });
    let chosen = rows.first().cloned().ok_or_else(|| anyhow!("no backend can build"))?;
    let _ = by_name;
    Ok(json!({"scenario": topo["scenario"], "board": board, "hub": hub_name, "client": client,
              "chosen": chosen, "candidates": rows}))
}

pub fn render(plan: &Value) -> String {
    let c = &plan["chosen"];
    let mb = |v: &Value| f(v) / 1e6;
    let mut s = format!(
        "plan [{}]: flash {} (hub {}), source on {}\n  build on {}: est {:.0} s; {:.1} MB to move after build ({})\n  candidates:\n",
        plan["scenario"].as_str().unwrap_or("-"), plan["board"].as_str().unwrap_or(""), plan["hub"].as_str().unwrap_or(""),
        plan["client"].as_str().unwrap_or(""), c["builder"].as_str().unwrap_or(""), f(&c["est_total_s"]), mb(&c["bytes_after_build"]),
        c["bundle"]["path"].as_str().unwrap_or("")
    );
    for r in plan["candidates"].as_array().into_iter().flatten() {
        s += &format!(
            "    {:<14} {:>7.0} s  src {:>8.1} MB [{}]  layers {:>8.1} MB [{}]  build {:>5.0} s  bundle {:>8.1} MB [{}]\n",
            r["builder"].as_str().unwrap_or(""), f(&r["est_total_s"]),
            mb(&r["source"]["bytes"]), r["source"]["path"].as_str().unwrap_or(""),
            mb(&r["layers"]["bytes"]), r["layers"]["path"].as_str().unwrap_or(""), f(&r["build_s"]),
            mb(&r["bundle"]["bytes"]), r["bundle"]["path"].as_str().unwrap_or("")
        );
    }
    s
}
