//! Writes a (modified) [`Fs`] back to a SquashFS image, reusing the original image's
//! compressed data wherever the data did not change.
//!
//! * Data blocks of unchanged files are copied byte-for-byte, in their original order
//!   (files that shared blocks through de-duplication keep sharing them).
//! * A changed file is written where its old data was. Each of its blocks that is
//!   still identical to the original block is copied; only differing blocks are
//!   recompressed.
//! * Fragment blocks are copied unless one of their files changed. Such a fragment is
//!   rebuilt from its unchanged residents plus (if it still fits) the changed tail.
//! * Inode, directory, fragment and id tables are regenerated.

use std::collections::{BTreeMap, HashMap};

use super::read::Image;
use super::*;
use crate::{err, lzo, Result};

#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Data blocks and fragments copied from the original image.
    pub reused_blocks: usize,
    /// Data blocks whose literal bytes were patched in place (size unchanged).
    pub patched_blocks: usize,
    /// Data blocks and fragments compressed anew.
    pub new_blocks: usize,
    /// Size of the image without the 4 KiB end padding.
    pub bytes_used: u64,
}

struct MetaWriter {
    out: Vec<u8>,
    buf: Vec<u8>,
}

impl MetaWriter {
    fn new() -> Self {
        MetaWriter {
            out: Vec::new(),
            buf: Vec::new(),
        }
    }

    fn pos(&self) -> InodeRef {
        InodeRef {
            block: self.out.len() as u32,
            offset: self.buf.len() as u16,
        }
    }

    fn write(&mut self, b: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(b);
        while self.buf.len() >= METADATA_SIZE {
            let rest = self.buf.split_off(METADATA_SIZE);
            let block = std::mem::replace(&mut self.buf, rest);
            self.flush(&block)?;
        }
        Ok(())
    }

    fn flush(&mut self, block: &[u8]) -> Result<()> {
        let c = lzo::compress(block)?;
        if c.len() < block.len() {
            self.out.extend_from_slice(&(c.len() as u16).to_le_bytes());
            self.out.extend_from_slice(&c);
        } else {
            self.out.extend_from_slice(&(block.len() as u16 | 0x8000).to_le_bytes());
            self.out.extend_from_slice(block);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>> {
        if !self.buf.is_empty() {
            let block = std::mem::take(&mut self.buf);
            self.flush(&block)?;
        }
        Ok(self.out)
    }
}

/// Writes `entries` as metadata blocks followed by the u64 lookup array; returns the
/// absolute position of the lookup array.
fn write_indexed(out: &mut Vec<u8>, entries: &[u8]) -> Result<u64> {
    let mut starts = Vec::new();
    for chunk in entries.chunks(METADATA_SIZE) {
        starts.push(out.len() as u64);
        let mut w = MetaWriter::new();
        w.write(chunk)?;
        out.extend_from_slice(&w.finish()?);
    }
    let lookup = out.len() as u64;
    for s in starts {
        out.extend_from_slice(&s.to_le_bytes());
    }
    Ok(lookup)
}

/// Compresses one data block; returns (stored bytes, size word).
fn compress_block(b: &[u8]) -> Result<(Vec<u8>, u32)> {
    let c = lzo::compress(b)?;
    if c.len() < b.len() {
        let n = c.len() as u32;
        Ok((c, n))
    } else {
        Ok((b.to_vec(), b.len() as u32 | BLOCK_UNCOMPRESSED))
    }
}

/// Where a changed file's tail ended up.
#[derive(Clone, Copy)]
struct TailPlace {
    fragment: u32,
    offset: u32,
}

struct Builder<'a, 'b> {
    img: &'a Image<'b>,
    fs: &'a Fs,
    bs: usize,
    out: Vec<u8>,
    /// New layout of every regular file.
    layouts: HashMap<usize, FileLayout>,
    stats: Stats,
}

impl Builder<'_, '_> {
    fn content(&self, n: usize) -> Option<&Vec<u8>> {
        match &self.fs.nodes[n].kind {
            Kind::File { new_content, .. } => new_content.as_ref(),
            _ => None,
        }
    }

    fn orig_layout(&self, n: usize) -> Option<&FileLayout> {
        match &self.fs.nodes[n].kind {
            Kind::File { layout, .. } => layout.as_ref(),
            _ => None,
        }
    }

    /// Writes the full blocks of a changed file at the current position.
    fn emit_changed(&mut self, n: usize, tail: Option<TailPlace>) -> Result<()> {
        let content = self.content(n).cloned().unwrap_or_default();
        let orig = self.orig_layout(n).cloned();
        let len = content.len();
        let nblocks = if tail.is_some() {
            len / self.bs
        } else {
            len.div_ceil(self.bs)
        };
        let blocks_start = self.out.len() as u64;
        let mut block_sizes = Vec::with_capacity(nblocks);
        let mut sparse = 0u64;
        let mut orig_pos = orig.as_ref().map_or(0, |o| o.blocks_start);
        for i in 0..nblocks {
            let chunk = &content[i * self.bs..((i + 1) * self.bs).min(len)];
            let orig_word = orig.as_ref().and_then(|o| o.block_sizes.get(i).copied());
            let mut reused = false;
            if let (Some(o), Some(w)) = (&orig, orig_word) {
                let orig_len = self.bs.min(o.file_size as usize - i * self.bs);
                if orig_len == chunk.len() && self.img.read_block(orig_pos, w, orig_len)? == chunk {
                    let n = (w & !BLOCK_UNCOMPRESSED) as usize;
                    self.out
                        .extend_from_slice(&self.img.data[orig_pos as usize..orig_pos as usize + n]);
                    block_sizes.push(w);
                    if w == 0 {
                        sparse += chunk.len() as u64;
                    }
                    self.stats.reused_blocks += 1;
                    reused = true;
                }
            }
            if !reused {
                // A small binary patch usually only touches literal bytes of the
                // original compressed block: edit those in place (same size).
                let in_place = match (&orig, orig_word) {
                    (Some(o), Some(w)) if w != 0 && self.bs.min(o.file_size as usize - i * self.bs) == chunk.len() => {
                        let n = (w & !BLOCK_UNCOMPRESSED) as usize;
                        let raw = &self.img.data[orig_pos as usize..orig_pos as usize + n];
                        if w & BLOCK_UNCOMPRESSED != 0 {
                            Some(chunk.to_vec())
                        } else {
                            lzo::patch_literals(raw, chunk)
                        }
                    }
                    _ => None,
                };
                if let Some(b) = in_place {
                    self.out.extend_from_slice(&b);
                    block_sizes.push(orig_word.unwrap());
                    self.stats.patched_blocks += 1;
                } else {
                    let (b, w) = compress_block(chunk)?;
                    self.out.extend_from_slice(&b);
                    block_sizes.push(w);
                    self.stats.new_blocks += 1;
                }
            }
            if let Some(w) = orig_word {
                orig_pos += (w & !BLOCK_UNCOMPRESSED) as u64;
            }
        }
        let (fragment, fragment_offset) = tail.map_or((NO_FRAGMENT, 0), |t| (t.fragment, t.offset));
        self.layouts.insert(
            n,
            FileLayout {
                blocks_start,
                block_sizes,
                fragment,
                fragment_offset,
                file_size: len as u64,
                sparse,
            },
        );
        Ok(())
    }
}

/// Serialises `fs` (a possibly modified copy of `img.fs`) into a new image.
pub fn rebuild(img: &Image, fs: &Fs) -> Result<(Vec<u8>, Stats)> {
    let bs = fs.sb.block_size as usize;
    let mut b = Builder {
        img,
        fs,
        bs,
        out: Vec::new(),
        layouts: HashMap::new(),
        stats: Stats::default(),
    };
    b.out.resize(Superblock::SIZE, 0);
    b.out.extend_from_slice(&fs.compressor_options);

    let files: Vec<usize> = (0..fs.nodes.len())
        .filter(|&n| matches!(fs.nodes[n].kind, Kind::File { .. }))
        .collect();
    let changed = |n: usize| {
        matches!(
            &fs.nodes[n].kind,
            Kind::File {
                new_content: Some(_),
                ..
            }
        )
    };

    // --- Fragment plan -------------------------------------------------------
    let mut residents: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for &n in &files {
        if let Some(l) = b.orig_layout(n) {
            if l.fragment != NO_FRAGMENT {
                residents.entry(l.fragment).or_default().push(n);
            }
        }
    }
    let mut rebuilt: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut new_frag_offset: HashMap<usize, u32> = HashMap::new(); // unchanged node -> offset
    let mut tails: HashMap<usize, TailPlace> = HashMap::new(); // changed node -> place
    for (&fi, nodes) in &residents {
        if !nodes.iter().any(|&n| changed(n)) {
            continue;
        }
        let old = img.fragment(fi)?;
        let mut content = Vec::new();
        let mut moved: HashMap<(u32, u64), u32> = HashMap::new();
        let mut sorted: Vec<usize> = nodes.iter().copied().filter(|&n| !changed(n)).collect();
        sorted.sort_by_key(|&n| (b.orig_layout(n).unwrap().fragment_offset, n));
        for n in sorted {
            let l = b.orig_layout(n).unwrap();
            let tail = l.file_size % bs as u64;
            let key = (l.fragment_offset, tail);
            let off = match moved.get(&key) {
                Some(&o) => o,
                None => {
                    let o = content.len() as u32;
                    let s = l.fragment_offset as usize;
                    content.extend_from_slice(
                        old.get(s..s + tail as usize)
                            .ok_or_else(|| err!("fragment too short"))?,
                    );
                    moved.insert(key, o);
                    o
                }
            };
            new_frag_offset.insert(n, off);
        }
        for &n in nodes.iter().filter(|&&n| changed(n)) {
            let c = b.content(n).unwrap();
            let tail = c.len() % bs;
            if tail > 0 && content.len() + tail <= bs {
                tails.insert(
                    n,
                    TailPlace {
                        fragment: fi,
                        offset: content.len() as u32,
                    },
                );
                content.extend_from_slice(&c[c.len() - tail..]);
            }
        }
        if content.is_empty() {
            // Nothing left in it; keep a valid (tiny) block so indices stay stable.
            content.push(0);
        }
        rebuilt.insert(fi, content);
    }

    // --- Data area, in original order -----------------------------------------
    enum Extent {
        Blocks(u64),
        Fragment(u32),
    }
    let mut by_start: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for &n in &files {
        if let Some(l) = b.orig_layout(n) {
            if l.blocks_len() > 0 {
                by_start.entry(l.blocks_start).or_default().push(n);
            }
        }
    }
    let mut extents: Vec<(u64, Extent)> = by_start.keys().map(|&s| (s, Extent::Blocks(s))).collect();
    extents.extend(
        fs.fragments
            .iter()
            .enumerate()
            .map(|(i, f)| (f.start, Extent::Fragment(i as u32))),
    );
    extents.sort_by_key(|e| e.0);

    let mut new_blocks_start: HashMap<u64, u64> = HashMap::new();
    let mut fragments = fs.fragments.clone();
    for (start, ext) in extents {
        match ext {
            Extent::Blocks(s) => {
                let nodes = &by_start[&s];
                let len = nodes
                    .iter()
                    .filter(|&&n| !changed(n))
                    .map(|&n| b.orig_layout(n).unwrap().blocks_len())
                    .max();
                if let Some(len) = len {
                    new_blocks_start.insert(s, b.out.len() as u64);
                    let src = img
                        .data
                        .get(s as usize..(s + len) as usize)
                        .ok_or_else(|| err!("data past end"))?;
                    b.out.extend_from_slice(src);
                    b.stats.reused_blocks += nodes
                        .iter()
                        .filter(|&&n| !changed(n))
                        .map(|&n| b.orig_layout(n).unwrap().block_sizes.len())
                        .max()
                        .unwrap_or(0);
                }
                for &n in nodes.iter().filter(|&&n| changed(n)) {
                    let tail = tails.get(&n).copied();
                    b.emit_changed(n, tail)?;
                }
            }
            Extent::Fragment(fi) => {
                let new_start = b.out.len() as u64;
                if let Some(content) = rebuilt.get(&fi) {
                    let (data, word) = compress_block(content)?;
                    b.out.extend_from_slice(&data);
                    fragments[fi as usize] = Fragment {
                        start: new_start,
                        size: word,
                    };
                    b.stats.new_blocks += 1;
                } else {
                    let f = fs.fragments[fi as usize];
                    let n = (f.size & !BLOCK_UNCOMPRESSED) as u64;
                    let src = img
                        .data
                        .get(start as usize..(start + n) as usize)
                        .ok_or_else(|| err!("fragment past end"))?;
                    b.out.extend_from_slice(src);
                    fragments[fi as usize] = Fragment {
                        start: new_start,
                        size: f.size,
                    };
                    b.stats.reused_blocks += 1;
                }
            }
        }
    }
    // Changed or new files that had no data blocks of their own go last.
    for &n in &files {
        if changed(n) && !b.layouts.contains_key(&n) {
            let tail = tails.get(&n).copied();
            b.emit_changed(n, tail)?;
        }
    }
    // Unchanged files: same blocks and fragment, new positions.
    for &n in &files {
        if changed(n) {
            continue;
        }
        let mut l = b
            .orig_layout(n)
            .cloned()
            .ok_or_else(|| err!("file without data layout"))?;
        if let Some(&s) = new_blocks_start.get(&l.blocks_start) {
            l.blocks_start = s;
        }
        if let Some(&o) = new_frag_offset.get(&n) {
            l.fragment_offset = o;
        }
        b.layouts.insert(n, l);
    }

    // --- Metadata ------------------------------------------------------------
    let mut parent: HashMap<usize, usize> = HashMap::new();
    let mut reachable = vec![false; fs.nodes.len()];
    let mut stack = vec![fs.root];
    while let Some(n) = stack.pop() {
        if std::mem::replace(&mut reachable[n], true) {
            continue;
        }
        if let Kind::Dir { children } = &fs.nodes[n].kind {
            for &(_, c) in children {
                parent.entry(c).or_insert(n);
                stack.push(c);
            }
        }
    }
    let inode_count = reachable.iter().filter(|&&r| r).count() as u32;
    let mut meta = Meta {
        fs,
        layouts: &b.layouts,
        parent: &parent,
        root_parent: inode_count + 1,
        inodes: MetaWriter::new(),
        dirs: MetaWriter::new(),
        refs: HashMap::new(),
    };
    let root = meta.node(fs.root)?;
    let Meta { inodes, dirs, .. } = meta;

    let mut sb = fs.sb.clone();
    sb.inode_count = inode_count;
    sb.root_inode = root.to_u64();
    sb.inode_table = b.out.len() as u64;
    b.out.extend_from_slice(&inodes.finish()?);
    sb.directory_table = b.out.len() as u64;
    b.out.extend_from_slice(&dirs.finish()?);
    let frag_entries: Vec<u8> = fragments
        .iter()
        .flat_map(|f| {
            [
                f.start.to_le_bytes().as_slice(),
                &f.size.to_le_bytes(),
                &0u32.to_le_bytes(),
            ]
            .concat()
        })
        .collect();
    sb.fragment_count = fragments.len() as u32;
    sb.fragment_table = write_indexed(&mut b.out, &frag_entries)?;
    sb.export_table = NO_TABLE;
    let id_entries: Vec<u8> = fs.ids.iter().flat_map(|i| i.to_le_bytes()).collect();
    sb.id_count = fs.ids.len() as u16;
    sb.id_table = write_indexed(&mut b.out, &id_entries)?;
    sb.xattr_table = NO_TABLE;
    sb.bytes_used = b.out.len() as u64;
    b.stats.bytes_used = sb.bytes_used;
    b.out[..Superblock::SIZE].copy_from_slice(&sb.to_bytes());
    let padded = b.out.len().div_ceil(4096) * 4096;
    b.out.resize(padded, 0);
    Ok((b.out, b.stats))
}

struct Meta<'a> {
    fs: &'a Fs,
    layouts: &'a HashMap<usize, FileLayout>,
    parent: &'a HashMap<usize, usize>,
    root_parent: u32,
    inodes: MetaWriter,
    dirs: MetaWriter,
    refs: HashMap<usize, InodeRef>,
}

fn basic_type(k: &Kind) -> u16 {
    match k {
        Kind::Dir { .. } => 1,
        Kind::File { .. } => 2,
        Kind::Symlink { .. } => 3,
    }
}

impl Meta<'_> {
    /// Writes `n` (children first) and returns its inode reference.
    fn node(&mut self, n: usize) -> Result<InodeRef> {
        if let Some(&r) = self.refs.get(&n) {
            return Ok(r);
        }
        let node = &self.fs.nodes[n];
        let mut body = Vec::new();
        let put32 = |b: &mut Vec<u8>, v: u32| b.extend_from_slice(&v.to_le_bytes());
        let ty = match &node.kind {
            Kind::Dir { children } => {
                let mut entries = Vec::with_capacity(children.len());
                for (name, c) in children {
                    let r = self.node(*c)?;
                    let child = &self.fs.nodes[*c];
                    entries.push((name.clone(), r, child.inode_number, basic_type(&child.kind)));
                }
                let listing = dir_listing(&entries)?;
                let at = self.dirs.pos();
                self.dirs.write(&listing)?;
                let file_size = listing.len() as u32 + 3;
                let parent = self
                    .parent
                    .get(&n)
                    .map_or(self.root_parent, |&p| self.fs.nodes[p].inode_number);
                if file_size <= 0xffff {
                    put32(&mut body, at.block);
                    put32(&mut body, node.nlink);
                    body.extend_from_slice(&(file_size as u16).to_le_bytes());
                    body.extend_from_slice(&at.offset.to_le_bytes());
                    put32(&mut body, parent);
                    1
                } else {
                    put32(&mut body, node.nlink);
                    put32(&mut body, file_size);
                    put32(&mut body, at.block);
                    put32(&mut body, parent);
                    body.extend_from_slice(&0u16.to_le_bytes()); // no directory index
                    body.extend_from_slice(&at.offset.to_le_bytes());
                    put32(&mut body, NO_XATTR);
                    8
                }
            }
            Kind::File { .. } => {
                let l = self
                    .layouts
                    .get(&n)
                    .ok_or_else(|| err!("missing layout for file inode"))?;
                let extended = node.extended
                    || node.nlink != 1
                    || l.sparse != 0
                    || l.blocks_start > u32::MAX as u64
                    || l.file_size > u32::MAX as u64;
                if extended {
                    body.extend_from_slice(&l.blocks_start.to_le_bytes());
                    body.extend_from_slice(&l.file_size.to_le_bytes());
                    body.extend_from_slice(&l.sparse.to_le_bytes());
                    put32(&mut body, node.nlink);
                    put32(&mut body, l.fragment);
                    put32(&mut body, l.fragment_offset);
                    put32(&mut body, NO_XATTR);
                } else {
                    put32(&mut body, l.blocks_start as u32);
                    put32(&mut body, l.fragment);
                    put32(&mut body, l.fragment_offset);
                    put32(&mut body, l.file_size as u32);
                }
                for &w in &l.block_sizes {
                    put32(&mut body, w);
                }
                if extended {
                    9
                } else {
                    2
                }
            }
            Kind::Symlink { target } => {
                put32(&mut body, node.nlink);
                put32(&mut body, target.len() as u32);
                body.extend_from_slice(target);
                if node.extended {
                    put32(&mut body, NO_XATTR);
                    10
                } else {
                    3
                }
            }
        };
        let mut inode = Vec::with_capacity(16 + body.len());
        for v in [ty, node.permissions, node.uid_index, node.gid_index] {
            inode.extend_from_slice(&v.to_le_bytes());
        }
        inode.extend_from_slice(&node.mtime.to_le_bytes());
        inode.extend_from_slice(&node.inode_number.to_le_bytes());
        inode.extend_from_slice(&body);
        let r = self.inodes.pos();
        self.inodes.write(&inode)?;
        self.refs.insert(n, r);
        Ok(r)
    }
}

/// Encodes directory entries (already sorted) into headers + entries.
fn dir_listing(entries: &[(Vec<u8>, InodeRef, u32, u16)]) -> Result<Vec<u8>> {
    let mut groups: Vec<(u32, u32, Vec<usize>)> = Vec::new();
    for (i, (_, r, num, _)) in entries.iter().enumerate() {
        let fits = groups.last().is_some_and(|(block, base, items)| {
            *block == r.block && items.len() < 256 && i16::try_from(*num as i64 - *base as i64).is_ok()
        });
        if fits {
            groups.last_mut().unwrap().2.push(i);
        } else {
            groups.push((r.block, *num, vec![i]));
        }
    }
    let mut out = Vec::new();
    for (block, base, items) in groups {
        out.extend_from_slice(&(items.len() as u32 - 1).to_le_bytes());
        out.extend_from_slice(&block.to_le_bytes());
        out.extend_from_slice(&base.to_le_bytes());
        for i in items {
            let (name, r, num, ty) = &entries[i];
            if name.is_empty() || name.len() > 256 {
                return Err(err!("invalid directory entry name"));
            }
            out.extend_from_slice(&r.offset.to_le_bytes());
            out.extend_from_slice(&((*num as i64 - base as i64) as i16).to_le_bytes());
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&(name.len() as u16 - 1).to_le_bytes());
            out.extend_from_slice(name);
        }
    }
    Ok(out)
}
