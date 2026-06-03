//! Minimal pure-Rust FDT v17 parser/emitter, scoped to the one DT mutation
//! we need: injecting PSCI `idle-states` into a QEMU-generated `virt`
//! machine DTB.
//!
//! QEMU's `-machine virt` does not emit `idle-states` or per-CPU
//! `cpu-idle-states` properties, so the kernel's PSCI cpuidle driver
//! (`drivers/cpuidle/cpuidle-psci.c`) never binds — even with
//! `CONFIG_ARM_PSCI_CPUIDLE=y`. Without a cpuidle driver, arm64 idle
//! falls back to bare WFI, which under HVF lets the vCPU thread bounce
//! through vmexit/vmenter rather than blocking on the WFI handler's
//! `pthread_cond_timedwait`. End result: a fully-idle 8-vCPU guest burns
//! ~670% host CPU.
//!
//! We dump QEMU's auto-generated DTB once (via `-machine virt,dumpdtb=`),
//! splice in the missing nodes, cache the patched copy, and pass it back
//! on the real launch with `-dtb`. With the driver bound, idle drops by
//! ~50% (the deeper PSCI suspend state stays cosmetic because HVF doesn't
//! implement CPU_SUSPEND any deeper than WFI today, but state0/WFI
//! through cpuidle is enough — the framework binding alone fixes the
//! vmexit-loop pattern).

use anyhow::{bail, Context, Result};

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_VERSION_OUT: u32 = 17;
const FDT_LAST_COMP_VERSION_OUT: u32 = 16;
const FDT_BEGIN_NODE: u32 = 0x1;
const FDT_END_NODE: u32 = 0x2;
const FDT_PROP: u32 = 0x3;
const FDT_NOP: u32 = 0x4;
const FDT_END: u32 = 0x9;

#[derive(Debug, Clone)]
pub struct Property {
    pub name: String,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub props: Vec<Property>,
    pub children: Vec<Node>,
}

impl Node {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), props: Vec::new(), children: Vec::new() }
    }

    pub fn set_prop(&mut self, name: &str, value: Vec<u8>) {
        if let Some(p) = self.props.iter_mut().find(|p| p.name == name) {
            p.value = value;
        } else {
            self.props.push(Property { name: name.to_string(), value });
        }
    }

    pub fn child_mut(&mut self, name: &str) -> Option<&mut Node> {
        self.children.iter_mut().find(|c| c.name == name)
    }
}

/// Parsed DTB: the root node tree, plus the original memory reservation
/// block (preserved verbatim on round-trip) and the original
/// `boot_cpuid_phys` header field.
pub struct Fdt {
    pub root: Node,
    pub mem_rsv: Vec<(u64, u64)>,
    pub boot_cpuid_phys: u32,
}

pub fn parse(data: &[u8]) -> Result<Fdt> {
    if data.len() < 40 {
        bail!("DTB too short: {} bytes", data.len());
    }
    let read_u32 = |off: usize| -> Result<u32> {
        let slice = data
            .get(off..off + 4)
            .with_context(|| format!("DTB header truncated at offset {off}"))?;
        Ok(u32::from_be_bytes(slice.try_into().unwrap()))
    };
    let magic = read_u32(0)?;
    if magic != FDT_MAGIC {
        bail!("bad DTB magic {magic:#x}, expected {FDT_MAGIC:#x}");
    }
    let totalsize = read_u32(4)? as usize;
    let off_dt_struct = read_u32(8)? as usize;
    let off_dt_strings = read_u32(12)? as usize;
    let off_mem_rsvmap = read_u32(16)? as usize;
    let version = read_u32(20)?;
    let boot_cpuid_phys = read_u32(28)?;
    let size_dt_strings = read_u32(32)? as usize;
    let size_dt_struct = read_u32(36)? as usize;
    if version < 16 {
        bail!("unsupported DTB version {version} (need v16+)");
    }
    if data.len() < totalsize {
        bail!("DTB truncated: header says {totalsize} bytes, got {}", data.len());
    }
    if off_dt_struct + size_dt_struct > data.len()
        || off_dt_strings + size_dt_strings > data.len()
    {
        bail!("DTB struct/strings offsets out of bounds");
    }

    let mut mem_rsv = Vec::new();
    let mut p = off_mem_rsvmap;
    loop {
        if p + 16 > data.len() {
            bail!("DTB memory reservation block truncated");
        }
        let addr = u64::from_be_bytes(data[p..p + 8].try_into().unwrap());
        let size = u64::from_be_bytes(data[p + 8..p + 16].try_into().unwrap());
        p += 16;
        if addr == 0 && size == 0 {
            break;
        }
        mem_rsv.push((addr, size));
    }

    let mut parser = Parser { data, pos: off_dt_struct, strings_base: off_dt_strings };
    let first = parser.read_u32()?;
    if first != FDT_BEGIN_NODE {
        bail!("DTB struct block must start with BEGIN_NODE, got {first:#x}");
    }
    let root = parser.read_node()?;
    let last = parser.read_u32()?;
    if last != FDT_END {
        bail!("DTB struct block missing FDT_END terminator, got {last:#x}");
    }
    Ok(Fdt { root, mem_rsv, boot_cpuid_phys })
}

struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
    strings_base: usize,
}

impl<'a> Parser<'a> {
    fn read_u32(&mut self) -> Result<u32> {
        let slice = self
            .data
            .get(self.pos..self.pos + 4)
            .with_context(|| format!("DTB truncated reading u32 at {}", self.pos))?;
        self.pos += 4;
        Ok(u32::from_be_bytes(slice.try_into().unwrap()))
    }

    fn read_cstr(&mut self) -> Result<String> {
        let start = self.pos;
        while self.pos < self.data.len() && self.data[self.pos] != 0 {
            self.pos += 1;
        }
        if self.pos >= self.data.len() {
            bail!("DTB cstr unterminated at offset {start}");
        }
        let s = std::str::from_utf8(&self.data[start..self.pos])
            .with_context(|| format!("non-utf8 name at offset {start}"))?
            .to_string();
        self.pos += 1;
        while self.pos % 4 != 0 {
            self.pos += 1;
        }
        Ok(s)
    }

    fn read_string_at(&self, off: usize) -> Result<String> {
        let start = self.strings_base + off;
        let mut end = start;
        while end < self.data.len() && self.data[end] != 0 {
            end += 1;
        }
        if end >= self.data.len() {
            bail!("DTB strings entry unterminated at offset {off}");
        }
        Ok(std::str::from_utf8(&self.data[start..end])
            .with_context(|| format!("non-utf8 prop name at strings offset {off}"))?
            .to_string())
    }

    fn read_node(&mut self) -> Result<Node> {
        let name = self.read_cstr()?;
        let mut node = Node::new(name);
        loop {
            let tok = self.read_u32()?;
            match tok {
                FDT_PROP => {
                    let len = self.read_u32()? as usize;
                    let nameoff = self.read_u32()? as usize;
                    let val = self
                        .data
                        .get(self.pos..self.pos + len)
                        .with_context(|| {
                            format!("DTB prop value truncated at offset {}", self.pos)
                        })?
                        .to_vec();
                    self.pos += len;
                    while self.pos % 4 != 0 {
                        self.pos += 1;
                    }
                    node.props.push(Property {
                        name: self.read_string_at(nameoff)?,
                        value: val,
                    });
                }
                FDT_BEGIN_NODE => {
                    let child = self.read_node()?;
                    node.children.push(child);
                }
                FDT_END_NODE => return Ok(node),
                FDT_NOP => {}
                other => bail!("unexpected DTB token {other:#x} at pos {}", self.pos - 4),
            }
        }
    }
}

struct Emitter {
    structs: Vec<u8>,
    strings: Vec<u8>,
}

impl Emitter {
    fn new() -> Self { Self { structs: Vec::new(), strings: Vec::new() } }

    fn intern(&mut self, name: &str) -> u32 {
        let bytes = name.as_bytes();
        let mut i = 0;
        while i < self.strings.len() {
            let mut j = i;
            while j < self.strings.len() && self.strings[j] != 0 {
                j += 1;
            }
            if &self.strings[i..j] == bytes {
                return i as u32;
            }
            i = j + 1;
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(bytes);
        self.strings.push(0);
        off
    }

    fn push_u32(&mut self, v: u32) { self.structs.extend_from_slice(&v.to_be_bytes()); }

    fn pad4(&mut self) {
        while self.structs.len() % 4 != 0 {
            self.structs.push(0);
        }
    }

    fn emit_node(&mut self, node: &Node) {
        self.push_u32(FDT_BEGIN_NODE);
        self.structs.extend_from_slice(node.name.as_bytes());
        self.structs.push(0);
        self.pad4();
        for p in &node.props {
            self.push_u32(FDT_PROP);
            self.push_u32(p.value.len() as u32);
            let off = self.intern(&p.name);
            self.push_u32(off);
            self.structs.extend_from_slice(&p.value);
            self.pad4();
        }
        for c in &node.children {
            self.emit_node(c);
        }
        self.push_u32(FDT_END_NODE);
    }
}

pub fn serialize(fdt: &Fdt) -> Vec<u8> {
    let mut em = Emitter::new();
    em.emit_node(&fdt.root);
    em.push_u32(FDT_END);

    let mut rsvbuf = Vec::new();
    for (a, s) in &fdt.mem_rsv {
        rsvbuf.extend_from_slice(&a.to_be_bytes());
        rsvbuf.extend_from_slice(&s.to_be_bytes());
    }
    rsvbuf.extend_from_slice(&[0u8; 16]); // terminator

    let header_size = 40usize;
    let off_mem_rsvmap = header_size;
    let off_dt_struct = off_mem_rsvmap + rsvbuf.len();
    let off_dt_strings = off_dt_struct + em.structs.len();
    let totalsize = off_dt_strings + em.strings.len();

    let mut out = Vec::with_capacity(totalsize);
    out.extend_from_slice(&FDT_MAGIC.to_be_bytes());
    out.extend_from_slice(&(totalsize as u32).to_be_bytes());
    out.extend_from_slice(&(off_dt_struct as u32).to_be_bytes());
    out.extend_from_slice(&(off_dt_strings as u32).to_be_bytes());
    out.extend_from_slice(&(off_mem_rsvmap as u32).to_be_bytes());
    out.extend_from_slice(&FDT_VERSION_OUT.to_be_bytes());
    out.extend_from_slice(&FDT_LAST_COMP_VERSION_OUT.to_be_bytes());
    out.extend_from_slice(&fdt.boot_cpuid_phys.to_be_bytes());
    out.extend_from_slice(&(em.strings.len() as u32).to_be_bytes());
    out.extend_from_slice(&(em.structs.len() as u32).to_be_bytes());
    out.extend_from_slice(&rsvbuf);
    out.extend_from_slice(&em.structs);
    out.extend_from_slice(&em.strings);
    out
}

fn max_phandle(node: &Node) -> u32 {
    let mut max = 0;
    fn walk(n: &Node, max: &mut u32) {
        for p in &n.props {
            if (p.name == "phandle" || p.name == "linux,phandle") && p.value.len() == 4 {
                let v = u32::from_be_bytes(p.value.as_slice().try_into().unwrap());
                if v > *max && v != u32::MAX {
                    *max = v;
                }
            }
        }
        for c in &n.children {
            walk(c, max);
        }
    }
    walk(node, &mut max);
    max
}

fn be32(v: u32) -> Vec<u8> { v.to_be_bytes().to_vec() }
fn strprop(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

/// Splice a single PSCI idle-state node into the root and add
/// `cpu-idle-states = <phandle>` to each `/cpus/cpu@N` for N in 0..smp.
///
/// Latency values are intentionally conservative. With aggressive thresholds
/// (entry=10, exit=20, min-residency=100) the kernel falls into a polling
/// code path for sub-100us idles and host CPU goes *up*, not down.
/// entry=100/exit=250/min-residency=1000 keeps cpuidle going through plain
/// WFI which HVF blocks cleanly on `pthread_cond_timedwait`. Confirmed
/// empirically: 670% → 275% on smp=8 idle.
///
/// Returns the phandle assigned to the new state node, for diagnostics.
pub fn patch_idle_states(fdt: &mut Fdt, smp: u32) -> Result<u32> {
    let phandle = max_phandle(&fdt.root) + 1;

    let mut idle_states = Node::new("idle-states");
    idle_states.set_prop("entry-method", strprop("psci"));

    let mut sleep = Node::new("cpu-sleep-0");
    sleep.set_prop("compatible", strprop("arm,idle-state"));
    sleep.set_prop("idle-state-name", strprop("cpu-sleep"));
    // PSCI v0.2+ power_state encoding for a CPU-level powerdown:
    // bit 16 = StateType (1 = powerdown), bits 31:24 = AffinityLevel (0 = CPU).
    sleep.set_prop("arm,psci-suspend-param", be32(0x0001_0000));
    sleep.set_prop("entry-latency-us", be32(100));
    sleep.set_prop("exit-latency-us", be32(250));
    sleep.set_prop("min-residency-us", be32(1000));
    sleep.set_prop("local-timer-stop", Vec::new());
    sleep.set_prop("phandle", be32(phandle));
    idle_states.children.push(sleep);
    fdt.root.children.push(idle_states);

    let cpus = fdt
        .root
        .child_mut("cpus")
        .context("DTB has no /cpus node — not a virt machine?")?;
    let mut patched = 0u32;
    for i in 0..smp {
        let name = format!("cpu@{i}");
        if let Some(cpu) = cpus.child_mut(&name) {
            cpu.set_prop("cpu-idle-states", be32(phandle));
            patched += 1;
        }
    }
    if patched == 0 {
        bail!("DTB has no /cpus/cpu@N nodes — refusing to patch");
    }
    Ok(phandle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal synthetic DTB with /cpus/cpu@0..cpu@N, round-trip
    /// it through parse → patch → serialize → parse, verify shape.
    fn synth_dtb(smp: u32) -> Vec<u8> {
        let mut root = Node::new("");
        root.set_prop("#address-cells", be32(2));
        root.set_prop("#size-cells", be32(2));
        let mut cpus = Node::new("cpus");
        cpus.set_prop("#address-cells", be32(1));
        cpus.set_prop("#size-cells", be32(0));
        for i in 0..smp {
            let mut cpu = Node::new(format!("cpu@{i}"));
            cpu.set_prop("device_type", strprop("cpu"));
            cpu.set_prop("compatible", strprop("arm,armv8"));
            cpu.set_prop("reg", be32(i));
            cpu.set_prop("enable-method", strprop("psci"));
            cpu.set_prop("phandle", be32(0x8000 + i));
            cpus.children.push(cpu);
        }
        root.children.push(cpus);
        let fdt = Fdt { root, mem_rsv: vec![], boot_cpuid_phys: 0 };
        serialize(&fdt)
    }

    #[test]
    fn round_trip_synthetic() {
        let bytes = synth_dtb(4);
        let fdt = parse(&bytes).unwrap();
        assert_eq!(fdt.root.children[0].name, "cpus");
        assert_eq!(fdt.root.children[0].children.len(), 4);
        assert_eq!(fdt.boot_cpuid_phys, 0);
    }

    #[test]
    fn patch_adds_idle_states_and_cpu_props() {
        let bytes = synth_dtb(4);
        let mut fdt = parse(&bytes).unwrap();
        let phandle = patch_idle_states(&mut fdt, 4).unwrap();
        // existing phandles go up to 0x8003 → new one should be 0x8004
        assert_eq!(phandle, 0x8004);

        let idle = fdt
            .root
            .children
            .iter()
            .find(|c| c.name == "idle-states")
            .expect("idle-states node missing");
        assert_eq!(idle.children[0].name, "cpu-sleep-0");

        let cpus = fdt.root.children.iter().find(|c| c.name == "cpus").unwrap();
        for cpu in &cpus.children {
            let cis = cpu
                .props
                .iter()
                .find(|p| p.name == "cpu-idle-states")
                .expect("cpu-idle-states missing on cpu node");
            assert_eq!(u32::from_be_bytes(cis.value.as_slice().try_into().unwrap()), phandle);
        }
    }

    #[test]
    fn patch_then_serialize_then_reparse_matches() {
        let bytes = synth_dtb(2);
        let mut fdt = parse(&bytes).unwrap();
        patch_idle_states(&mut fdt, 2).unwrap();
        let out = serialize(&fdt);
        let rt = parse(&out).unwrap();
        assert!(rt.root.children.iter().any(|c| c.name == "idle-states"));
        let cpus = rt.root.children.iter().find(|c| c.name == "cpus").unwrap();
        assert!(cpus.children.iter().all(|c| c.props.iter().any(|p| p.name == "cpu-idle-states")));
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut bytes = synth_dtb(1);
        bytes[0] = 0;
        assert!(parse(&bytes).is_err());
    }

    #[test]
    fn patch_fails_when_no_cpus_node() {
        let mut root = Node::new("");
        root.set_prop("#address-cells", be32(2));
        let fdt_in = Fdt { root, mem_rsv: vec![], boot_cpuid_phys: 0 };
        let bytes = serialize(&fdt_in);
        let mut fdt = parse(&bytes).unwrap();
        assert!(patch_idle_states(&mut fdt, 4).is_err());
    }

    #[test]
    fn boot_cpuid_phys_preserved() {
        let mut root = Node::new("");
        let mut cpus = Node::new("cpus");
        let mut cpu = Node::new("cpu@0");
        cpu.set_prop("reg", be32(0));
        cpus.children.push(cpu);
        root.children.push(cpus);
        let fdt_in = Fdt { root, mem_rsv: vec![], boot_cpuid_phys: 0x42 };
        let bytes = serialize(&fdt_in);
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed.boot_cpuid_phys, 0x42);
    }
}
