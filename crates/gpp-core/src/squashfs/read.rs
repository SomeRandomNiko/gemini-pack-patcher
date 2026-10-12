use std::cell::RefCell;
use std::collections::HashMap;

use super::*;
use crate::{err, lzo, Result};

/// A parsed SquashFS image: the raw bytes plus the decoded tree.
pub struct Image<'a> {
    pub data: &'a [u8],
    pub fs: Fs,
    fragment_cache: RefCell<HashMap<u32, std::rc::Rc<Vec<u8>>>>,
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let s = self
            .b
            .get(self.p..self.p + n)
            .ok_or_else(|| err!("SquashFS metadata truncated"))?;
        self.p += n;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

/// A metadata table decompressed into one buffer, with the uncompressed position of
/// every metadata block (keyed by its offset relative to the table start).
struct Table {
    bytes: Vec<u8>,
    blocks: HashMap<u32, usize>,
}

impl Table {
    fn at(&self, r: InodeRef) -> Result<usize> {
        let base = self
            .blocks
            .get(&r.block)
            .ok_or_else(|| err!("SquashFS: bad metadata reference {:#x}", r.block))?;
        Ok(base + r.offset as usize)
    }
}

/// Reads the metadata block at `pos`; returns its contents and the next position.
pub(super) fn metablock(data: &[u8], pos: usize) -> Result<(Vec<u8>, usize)> {
    let h = data
        .get(pos..pos + 2)
        .ok_or_else(|| err!("SquashFS: metadata block past end"))?;
    let h = u16::from_le_bytes([h[0], h[1]]);
    let len = (h & 0x7fff) as usize;
    let raw = data
        .get(pos + 2..pos + 2 + len)
        .ok_or_else(|| err!("SquashFS: metadata block past end"))?;
    let bytes = if h & 0x8000 != 0 {
        raw.to_vec()
    } else {
        lzo::decompress(raw, METADATA_SIZE)?
    };
    Ok((bytes, pos + 2 + len))
}

fn read_table(data: &[u8], start: u64, end: u64) -> Result<Table> {
    let mut t = Table {
        bytes: Vec::new(),
        blocks: HashMap::new(),
    };
    let mut pos = start as usize;
    while pos < end as usize {
        t.blocks.insert((pos - start as usize) as u32, t.bytes.len());
        let (b, next) = metablock(data, pos)?;
        t.bytes.extend_from_slice(&b);
        pos = next;
    }
    Ok(t)
}

/// Reads a table stored as metadata blocks indexed by a lookup array of u64s.
fn read_indexed(data: &[u8], lookup: u64, entries: usize, entry_size: usize) -> Result<(Vec<u8>, Vec<u64>)> {
    let nblocks = (entries * entry_size).div_ceil(METADATA_SIZE);
    let mut r = Reader {
        b: data,
        p: lookup as usize,
    };
    let mut starts = Vec::with_capacity(nblocks);
    let mut bytes = Vec::new();
    for _ in 0..nblocks {
        let s = r.u64()?;
        starts.push(s);
        bytes.extend_from_slice(&metablock(data, s as usize)?.0);
    }
    if bytes.len() < entries * entry_size {
        return Err(err!("SquashFS: lookup table truncated"));
    }
    Ok((bytes, starts))
}

pub(super) fn parse_superblock(data: &[u8]) -> Result<Superblock> {
    let mut r = Reader { b: data, p: 0 };
    if r.u32()? != MAGIC {
        return Err(err!("not a SquashFS image"));
    }
    let sb = Superblock {
        inode_count: r.u32()?,
        mkfs_time: r.u32()?,
        block_size: r.u32()?,
        fragment_count: r.u32()?,
        compression: r.u16()?,
        block_log: r.u16()?,
        flags: r.u16()?,
        id_count: r.u16()?,
        root_inode: {
            let (major, minor) = (r.u16()?, r.u16()?);
            if (major, minor) != (4, 0) {
                return Err(err!("unsupported SquashFS version {major}.{minor}"));
            }
            r.u64()?
        },
        bytes_used: r.u64()?,
        id_table: r.u64()?,
        xattr_table: r.u64()?,
        inode_table: r.u64()?,
        directory_table: r.u64()?,
        fragment_table: r.u64()?,
        export_table: r.u64()?,
    };
    if sb.compression != COMPRESSION_LZO {
        return Err(err!(
            "unsupported SquashFS compression {} (only LZO is supported)",
            sb.compression
        ));
    }
    let unsupported = flags::UNCOMPRESSED_INODES
        | flags::UNCOMPRESSED_DATA
        | flags::UNCOMPRESSED_FRAGMENTS
        | flags::NO_FRAGMENTS
        | flags::EXPORTABLE
        | flags::UNCOMPRESSED_IDS;
    if sb.flags & unsupported != 0 || sb.xattr_table != NO_TABLE {
        return Err(err!("unsupported SquashFS options (flags {:#x})", sb.flags));
    }
    if sb.block_size != 1 << sb.block_log || sb.block_size > 1 << 20 || sb.block_size < 4096 {
        return Err(err!("invalid SquashFS block size"));
    }
    if sb.bytes_used as usize > data.len() {
        return Err(err!("SquashFS image is truncated"));
    }
    Ok(sb)
}

impl<'a> Image<'a> {
    pub fn open(data: &'a [u8]) -> Result<Image<'a>> {
        let sb = parse_superblock(data)?;
        let mut pos = Superblock::SIZE;
        let mut compressor_options = Vec::new();
        if sb.flags & flags::COMPRESSOR_OPTIONS != 0 {
            let (_, next) = metablock(data, pos)?;
            compressor_options = data[pos..next].to_vec();
            pos = next;
        }
        let _ = pos;

        let (frag_bytes, frag_starts) = read_indexed(data, sb.fragment_table, sb.fragment_count as usize, 16)?;
        let fragments = (0..sb.fragment_count as usize)
            .map(|i| {
                let e = &frag_bytes[i * 16..i * 16 + 16];
                Fragment {
                    start: u64::from_le_bytes(e[0..8].try_into().unwrap()),
                    size: u32::from_le_bytes(e[8..12].try_into().unwrap()),
                }
            })
            .collect();
        let (id_bytes, id_starts) = read_indexed(data, sb.id_table, sb.id_count as usize, 4)?;
        let ids = (0..sb.id_count as usize)
            .map(|i| u32::from_le_bytes(id_bytes[i * 4..i * 4 + 4].try_into().unwrap()))
            .collect();

        // The directory table ends where the next table begins.
        let dir_end = frag_starts
            .iter()
            .chain(&id_starts)
            .chain(&[sb.fragment_table, sb.id_table, sb.bytes_used])
            .copied()
            .filter(|&p| p > sb.directory_table)
            .min()
            .unwrap_or(sb.bytes_used);
        let inodes = read_table(data, sb.inode_table, sb.directory_table)?;
        let dirs = read_table(data, sb.directory_table, dir_end)?;

        let mut p = Parser {
            inodes: &inodes,
            dirs: &dirs,
            block_size: sb.block_size,
            nodes: Vec::new(),
            seen: HashMap::new(),
        };
        let root = p.node(InodeRef::from_u64(sb.root_inode), 0)?;
        let nodes = p.nodes;
        let fs = Fs {
            sb,
            compressor_options,
            ids,
            fragments,
            nodes,
            root,
        };
        Ok(Image {
            data,
            fs,
            fragment_cache: RefCell::new(HashMap::new()),
        })
    }

    /// Reads one data block (`size_word` as stored in the inode) of `len` bytes.
    pub fn read_block(&self, start: u64, size_word: u32, len: usize) -> Result<Vec<u8>> {
        if size_word == 0 {
            return Ok(vec![0; len]);
        }
        let n = (size_word & !BLOCK_UNCOMPRESSED) as usize;
        let raw = self
            .data
            .get(start as usize..start as usize + n)
            .ok_or_else(|| err!("SquashFS: data block past end"))?;
        let b = if size_word & BLOCK_UNCOMPRESSED != 0 {
            raw.to_vec()
        } else {
            lzo::decompress(raw, self.fs.sb.block_size as usize)?
        };
        if b.len() != len {
            return Err(err!("SquashFS: data block has {} bytes, expected {len}", b.len()));
        }
        Ok(b)
    }

    pub fn fragment(&self, index: u32) -> Result<std::rc::Rc<Vec<u8>>> {
        if let Some(f) = self.fragment_cache.borrow().get(&index) {
            return Ok(f.clone());
        }
        let f = self
            .fs
            .fragments
            .get(index as usize)
            .ok_or_else(|| err!("SquashFS: bad fragment index {index}"))?;
        let n = (f.size & !BLOCK_UNCOMPRESSED) as usize;
        let raw = self
            .data
            .get(f.start as usize..f.start as usize + n)
            .ok_or_else(|| err!("SquashFS: fragment past end"))?;
        let b = std::rc::Rc::new(if f.size & BLOCK_UNCOMPRESSED != 0 {
            raw.to_vec()
        } else {
            lzo::decompress(raw, self.fs.sb.block_size as usize)?
        });
        self.fragment_cache.borrow_mut().insert(index, b.clone());
        Ok(b)
    }

    /// Reads a file's data as laid out in this image.
    pub fn read_layout(&self, l: &FileLayout) -> Result<Vec<u8>> {
        let size = l.file_size as usize;
        let bs = self.fs.sb.block_size as usize;
        let mut out = Vec::with_capacity(size);
        let mut pos = l.blocks_start;
        for &w in &l.block_sizes {
            let want = bs.min(size - out.len());
            out.extend_from_slice(&self.read_block(pos, w, want)?);
            pos += (w & !BLOCK_UNCOMPRESSED) as u64;
        }
        if l.fragment != NO_FRAGMENT {
            let f = self.fragment(l.fragment)?;
            let tail = size - out.len();
            let off = l.fragment_offset as usize;
            out.extend_from_slice(
                f.get(off..off + tail)
                    .ok_or_else(|| err!("SquashFS: fragment too short"))?,
            );
        }
        if out.len() != size {
            return Err(err!("SquashFS: file data has {} bytes, expected {size}", out.len()));
        }
        Ok(out)
    }

    /// The content of a regular file node of `fs` (new content if it was replaced).
    pub fn content(&self, fs: &Fs, node: usize) -> Result<Vec<u8>> {
        match &fs.nodes[node].kind {
            Kind::File {
                new_content: Some(c), ..
            } => Ok(c.clone()),
            Kind::File { layout: Some(l), .. } => self.read_layout(l),
            Kind::File { .. } => Ok(Vec::new()),
            _ => Err(err!("not a regular file")),
        }
    }
}

struct Parser<'a> {
    inodes: &'a Table,
    dirs: &'a Table,
    block_size: u32,
    nodes: Vec<Node>,
    seen: HashMap<InodeRef, usize>,
}

impl Parser<'_> {
    fn node(&mut self, at: InodeRef, depth: usize) -> Result<usize> {
        if let Some(&n) = self.seen.get(&at) {
            return Ok(n);
        }
        if depth > 64 {
            return Err(err!("SquashFS: directory tree too deep"));
        }
        let mut r = Reader {
            b: &self.inodes.bytes,
            p: self.inodes.at(at)?,
        };
        let ty = r.u16()?;
        let permissions = r.u16()?;
        let uid_index = r.u16()?;
        let gid_index = r.u16()?;
        let mtime = r.u32()?;
        let inode_number = r.u32()?;
        let (kind, nlink, extended, xattr, dir) = match ty {
            1 => {
                let start = r.u32()?;
                let nlink = r.u32()?;
                let size = r.u16()? as u32;
                let offset = r.u16()?;
                let _parent = r.u32()?;
                (None, nlink, false, NO_XATTR, Some((start, offset, size)))
            }
            8 => {
                let nlink = r.u32()?;
                let size = r.u32()?;
                let start = r.u32()?;
                let _parent = r.u32()?;
                let _index_count = r.u16()?;
                let offset = r.u16()?;
                let xattr = r.u32()?;
                (None, nlink, true, xattr, Some((start, offset, size)))
            }
            2 | 9 => {
                let (blocks_start, fragment, fragment_offset, file_size, sparse, nlink, xattr);
                if ty == 2 {
                    blocks_start = r.u32()? as u64;
                    fragment = r.u32()?;
                    fragment_offset = r.u32()?;
                    file_size = r.u32()? as u64;
                    (sparse, nlink, xattr) = (0, 1, NO_XATTR);
                } else {
                    blocks_start = r.u64()?;
                    file_size = r.u64()?;
                    sparse = r.u64()?;
                    nlink = r.u32()?;
                    fragment = r.u32()?;
                    fragment_offset = r.u32()?;
                    xattr = r.u32()?;
                }
                let bs = self.block_size as u64;
                let count = if fragment == NO_FRAGMENT {
                    file_size.div_ceil(bs)
                } else {
                    file_size / bs
                };
                let block_sizes = (0..count).map(|_| r.u32()).collect::<Result<Vec<_>>>()?;
                let layout = FileLayout {
                    blocks_start,
                    block_sizes,
                    fragment,
                    fragment_offset,
                    file_size,
                    sparse,
                };
                (
                    Some(Kind::File {
                        layout: Some(layout),
                        new_content: None,
                    }),
                    nlink,
                    ty == 9,
                    xattr,
                    None,
                )
            }
            3 | 10 => {
                let nlink = r.u32()?;
                let len = r.u32()? as usize;
                let target = r.take(len)?.to_vec();
                let xattr = if ty == 10 { r.u32()? } else { NO_XATTR };
                (Some(Kind::Symlink { target }), nlink, ty == 10, xattr, None)
            }
            _ => return Err(err!("SquashFS: unsupported inode type {ty} (device/fifo/socket)")),
        };
        if xattr != NO_XATTR {
            return Err(err!("SquashFS: extended attributes are not supported"));
        }
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind: kind.unwrap_or(Kind::Dir { children: Vec::new() }),
            permissions,
            uid_index,
            gid_index,
            mtime,
            inode_number,
            nlink,
            extended,
        });
        self.seen.insert(at, id);
        if let Some((start, offset, size)) = dir {
            let children = self.dir_entries(start, offset, size, depth)?;
            self.nodes[id].kind = Kind::Dir { children };
        }
        Ok(id)
    }

    fn dir_entries(&mut self, start: u32, offset: u16, size: u32, depth: usize) -> Result<Vec<(Vec<u8>, usize)>> {
        let mut children: Vec<(Vec<u8>, InodeRef)> = Vec::new();
        if size <= 3 {
            return Ok(Vec::new());
        }
        let begin = self.dirs.at(InodeRef { block: start, offset })?;
        let end = begin + size as usize - 3;
        let mut r = Reader {
            b: &self.dirs.bytes,
            p: begin,
        };
        while r.p < end {
            let count = r.u32()? as usize + 1;
            let block = r.u32()?;
            let _base = r.u32()?;
            if count > 256 {
                return Err(err!("SquashFS: corrupt directory header"));
            }
            for _ in 0..count {
                let offset = r.u16()?;
                let _delta = r.u16()?;
                let _ty = r.u16()?;
                let len = r.u16()? as usize + 1;
                let name = r.take(len)?.to_vec();
                children.push((name, InodeRef { block, offset }));
            }
        }
        children
            .into_iter()
            .map(|(name, at)| Ok((name, self.node(at, depth + 1)?)))
            .collect()
    }
}
