//! Identify a stock pack, apply selected patches, verify the result.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::catalog::{catalog, Action, FirmwareDef};
use crate::pack::{self, Layout};
use crate::squashfs::{self, write::rebuild, Fs, Image, Kind};
use crate::{err, sha256_hex, Progress, Result};

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    pub fn new(name: &str, ok: bool, detail: impl Into<String>) -> Check {
        Check {
            name: name.to_string(),
            ok,
            detail: detail.into(),
        }
    }
    pub fn fail(name: &str, detail: impl Into<String>) -> Check {
        Check::new(name, false, detail)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Identification {
    pub sha256: String,
    pub size: usize,
    /// The matching definition, if this is a supported stock firmware.
    pub firmware: Option<&'static FirmwareDef>,
}

pub fn identify(data: &[u8]) -> Identification {
    let sha256 = sha256_hex(data);
    let firmware = catalog().iter().find(|f| f.sha256 == sha256);
    Identification {
        sha256,
        size: data.len(),
        firmware,
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Selection {
    pub patches: Vec<SelectedPatch>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SelectedPatch {
    pub id: String,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub firmware: String,
    pub applied: Vec<String>,
    /// Paths (per partition) whose content changed or that were added.
    pub changed_files: Vec<String>,
    /// Unused bytes left in each rewritten partition.
    pub free_bytes: BTreeMap<String, u64>,
    pub checks: Vec<Check>,
    pub ok: bool,
    pub output_sha256: String,
}

pub struct ApplyOutput {
    pub data: Vec<u8>,
    pub report: Report,
}

/// Checks the selection against the definition and expands it into concrete actions
/// (components first, then patches, in definition order).
fn plan(def: &FirmwareDef, sel: &Selection) -> Result<(Vec<String>, Vec<Action>)> {
    if sel.patches.is_empty() {
        return Err(err!("no patches selected"));
    }
    let mut chosen = HashSet::new();
    for s in &sel.patches {
        let p = def
            .patch(&s.id)
            .ok_or_else(|| err!("unknown patch '{}' for {}", s.id, def.name))?;
        if !chosen.insert(p.id.as_str()) {
            return Err(err!("patch '{}' selected twice", s.id));
        }
        for q in &p.params {
            let v = s
                .params
                .get(&q.id)
                .ok_or_else(|| err!("{}: missing value for {}", p.title, q.label))?;
            q.validate(v)?;
        }
        if let Some(k) = s.params.keys().find(|k| !p.params.iter().any(|q| &q.id == *k)) {
            return Err(err!("{}: unknown parameter '{k}'", p.title));
        }
    }
    let selected: Vec<_> = def
        .patches
        .iter()
        .filter(|p| chosen.contains(p.id.as_str()))
        .map(|p| (p, &sel.patches.iter().find(|s| s.id == p.id).unwrap().params))
        .collect();

    let mut collections: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (p, params) in &selected {
        for c in &p.contributions {
            let v = crate::catalog::render(&c.value, &|n| params.get(n).cloned())?;
            collections.entry(c.collection.as_str()).or_default().push(v);
        }
    }
    let collect = |name: &str| -> Option<String> {
        let c = name.strip_prefix("collect:")?;
        Some(collections.get(c).map(|v| v.join("\n")).unwrap_or_default())
    };

    let mut actions = Vec::new();
    let mut components = Vec::new();
    for (p, _) in &selected {
        for r in &p.requires {
            if !components.contains(r) {
                components.push(r.clone());
            }
        }
    }
    for c in &components {
        for a in &def.components[c].actions {
            actions.push(a.render(&collect)?);
        }
    }
    for (p, params) in &selected {
        for a in &p.actions {
            actions.push(a.render(&|n| params.get(n).cloned().or_else(|| collect(n)))?);
        }
    }
    Ok((selected.iter().map(|(p, _)| p.id.clone()).collect(), actions))
}

fn file_node(fs: &Fs, path: &str) -> Result<usize> {
    let n = fs
        .lookup(path)
        .ok_or_else(|| err!("file '{path}' not found in firmware"))?;
    match fs.nodes[n].kind {
        Kind::File { .. } => Ok(n),
        _ => Err(err!("'{path}' is not a regular file")),
    }
}

fn apply_action(img: &Image, fs: &mut Fs, a: &Action) -> Result<String> {
    match a {
        Action::BytePatch {
            file,
            offset,
            expect,
            replace,
            ..
        } => {
            let n = file_node(fs, file)?;
            let mut c = img.content(fs, n)?;
            let (expect, replace) = (hex::decode(expect).unwrap(), hex::decode(replace).unwrap());
            let at = *offset as usize;
            let cur = c
                .get(at..at + expect.len())
                .ok_or_else(|| err!("{file}: offset {at:#x} is past the end"))?;
            if cur != expect.as_slice() {
                return Err(err!(
                    "{file} @ {at:#x}: found {}, expected {} (wrong firmware or already patched)",
                    hex::encode(cur),
                    hex::encode(&expect)
                ));
            }
            c[at..at + replace.len()].copy_from_slice(&replace);
            fs.set_content(n, c)?;
            Ok(file.clone())
        }
        Action::ReplaceText {
            file,
            find,
            replace,
            count,
            ..
        } => {
            let n = file_node(fs, file)?;
            let c = String::from_utf8(img.content(fs, n)?).map_err(|_| err!("{file} is not a text file"))?;
            let found = c.matches(find.as_str()).count();
            if found != *count {
                return Err(err!(
                    "{file}: expected {count} occurrence(s) of {find:?}, found {found}"
                ));
            }
            fs.set_content(n, c.replace(find.as_str(), replace).into_bytes())?;
            Ok(file.clone())
        }
        Action::AddFile {
            path, mode, content, ..
        } => {
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
            let parent = fs
                .lookup(dir)
                .ok_or_else(|| err!("directory '{dir}' not found in firmware"))?;
            fs.add_file(parent, name, *mode, content.clone().into_bytes())?;
            Ok(path.clone())
        }
    }
}

/// Compares the rewritten image against the expected tree: same paths, metadata and
/// contents for every entry.
fn compare_trees(orig: &Image, expected: &Fs, new: &Image) -> Result<String> {
    let (a, b) = (expected.walk(), new.fs.walk());
    if a.len() != b.len() {
        return Err(err!("{} entries expected, image has {}", a.len(), b.len()));
    }
    let (mut files, mut dirs, mut links) = (0, 0, 0);
    for ((pa, na), (pb, nb)) in a.iter().zip(&b) {
        let (x, y) = (&expected.nodes[*na], &new.fs.nodes[*nb]);
        let shown = if pa.is_empty() { "/" } else { pa.as_str() };
        if pa != pb {
            return Err(err!("entry mismatch: {pa} vs {pb}"));
        }
        let ids = |f: &Fs, i: u16| f.ids.get(i as usize).copied();
        if x.type_name() != y.type_name()
            || x.permissions != y.permissions
            || ids(expected, x.uid_index) != ids(&new.fs, y.uid_index)
            || ids(expected, x.gid_index) != ids(&new.fs, y.gid_index)
            || x.mtime != y.mtime
            || x.nlink != y.nlink
        {
            return Err(err!("{shown}: metadata differs"));
        }
        match (&x.kind, &y.kind) {
            (Kind::File { .. }, Kind::File { layout: Some(l), .. }) => {
                if orig.content(expected, *na)? != new.read_layout(l)? {
                    return Err(err!("{shown}: content differs"));
                }
                files += 1;
            }
            (Kind::Symlink { target: t1 }, Kind::Symlink { target: t2 }) => {
                if t1 != t2 {
                    return Err(err!("{shown}: symlink target differs"));
                }
                links += 1;
            }
            (Kind::Dir { .. }, Kind::Dir { .. }) => dirs += 1,
            _ => return Err(err!("{shown}: type differs")),
        }
    }
    Ok(format!("{files} files, {dirs} directories, {links} symlinks match"))
}

/// Applies `sel` to the stock pack `input` and verifies the result.
pub fn apply(input: &[u8], sel: &Selection, progress: Progress) -> Result<ApplyOutput> {
    progress("Identifying firmware", 0.0);
    let id = identify(input);
    let def = id.firmware.ok_or_else(|| {
        err!(
            "This file is not a supported stock firmware (SHA-256 {}). Use the original, unmodified GEMINI_PACK.BIN.",
            id.sha256
        )
    })?;
    let (applied, actions) = plan(def, sel)?;

    progress("Checking stock file integrity", 0.1);
    let bad: Vec<_> = pack::validate(input).into_iter().filter(|c| !c.ok).collect();
    if let Some(c) = bad.first() {
        return Err(err!(
            "stock file failed its own integrity check ({}: {})",
            c.name,
            c.detail
        ));
    }
    let layout = Layout::parse(input)?;
    let mut out = input.to_vec();
    let mut checks = Vec::new();
    let mut changed_files = Vec::new();
    let mut free_bytes = BTreeMap::new();

    let mut partitions: Vec<&str> = Vec::new();
    for a in &actions {
        if !partitions.contains(&a.partition()) {
            partitions.push(a.partition());
        }
    }
    for part in &partitions {
        progress(&format!("Patching {part}"), 0.2);
        let raw = layout.read(input, part)?;
        let img = Image::open(&raw).map_err(|e| err!("{part}: {e}"))?;
        let mut fs = img.fs.clone();
        let mut touched = Vec::new();
        for a in actions.iter().filter(|a| a.partition() == *part) {
            let f = apply_action(&img, &mut fs, a)?;
            if !touched.contains(&f) {
                touched.push(f);
            }
        }
        progress(&format!("Rebuilding {part} file system"), 0.35);
        let (mut new, stats) = rebuild(&img, &fs)?;
        let capacity = raw.len();
        if stats.bytes_used as usize > capacity {
            return Err(err!(
                "the patched {part} file system is {} bytes too large for its partition",
                stats.bytes_used as usize - capacity
            ));
        }
        free_bytes.insert(part.to_string(), capacity as u64 - stats.bytes_used);
        new.resize(capacity, 0);

        progress(&format!("Verifying {part} file system"), 0.55);
        let check = Image::open(&new).and_then(|n| {
            if n.fs.sb.block_size != img.fs.sb.block_size || n.fs.sb.flags != img.fs.sb.flags {
                return Err(err!("superblock options changed"));
            }
            compare_trees(&img, &fs, &n)
        });
        checks.push(match check {
            Ok(d) => Check::new(&format!("{part}: file system contents"), true, d),
            Err(e) => Check::fail(&format!("{part}: file system contents"), e.0),
        });
        checks.push(Check::new(
            &format!("{part}: fits partition"),
            true,
            format!(
                "{} of {} bytes used, {} free; {} blocks reused, {} patched in place, {} recompressed",
                stats.bytes_used,
                capacity,
                capacity as u64 - stats.bytes_used,
                stats.reused_blocks,
                stats.patched_blocks,
                stats.new_blocks
            ),
        ));
        layout.write(&mut out, part, &new)?;
        changed_files.extend(touched.into_iter().map(|f| format!("{part}:/{f}")));
    }

    progress("Updating checksums", 0.8);
    layout.fix_checksums(&mut out)?;

    progress("Final verification", 0.85);
    checks.extend(pack::validate(&out));
    checks.push(unchanged_outside(input, &out, &layout, &partitions));
    for part in &partitions {
        let written = layout.read(&out, part)?;
        let ok = squashfs::Image::open(&written).is_ok();
        checks.push(Check::new(
            &format!("{part}: readable from output file"),
            ok,
            if ok { "OK" } else { "unreadable" },
        ));
    }
    let ok = checks.iter().all(|c| c.ok);
    progress("Done", 1.0);
    let output_sha256 = sha256_hex(&out);
    let report = Report {
        firmware: def.id.clone(),
        applied,
        changed_files,
        free_bytes,
        checks,
        ok,
        output_sha256,
    };
    Ok(ApplyOutput { data: out, report })
}

/// Every byte outside the rewritten partitions and the checksum fields is unchanged.
fn unchanged_outside(a: &[u8], b: &[u8], layout: &Layout, parts: &[&str]) -> Check {
    let name = "Everything else unchanged";
    if a.len() != b.len() {
        return Check::fail(name, "file size changed");
    }
    let h = layout.script_header;
    let mut allowed: Vec<(usize, usize)> = vec![(0xea, 0xea + 32), (h + 4, h + 8), (h + 24, h + 28)];
    for p in layout.partitions.iter().filter(|p| parts.contains(&p.name.as_str())) {
        for c in &p.chunks {
            allowed.push((c.offset, c.offset + c.len));
            allowed.push((c.md5_pos, c.md5_pos + 32));
        }
    }
    allowed.sort();
    let mut pos = 0;
    for (s, e) in allowed.iter().copied().chain([(a.len(), a.len())]) {
        if s > pos && a[pos..s] != b[pos..s] {
            let at = pos + a[pos..s].iter().zip(&b[pos..s]).position(|(x, y)| x != y).unwrap();
            return Check::fail(name, format!("unexpected change at {at:#x}"));
        }
        pos = pos.max(e);
    }
    Check::new(
        name,
        true,
        "all other partitions and headers are byte-identical to stock",
    )
}
