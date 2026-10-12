//! Minimal SquashFS 4.0 (LZO) reader and a rewriter that preserves the original image.
//!
//! The head unit's `spapp` partition has only a few KB of free space, so a generic
//! "extract and rebuild" with a different compressor would not fit. Instead
//! [`write::rebuild`] copies every unchanged data block and fragment byte-for-byte
//! from the original image and only compresses what actually changed; the metadata
//! tables (inodes, directories, fragment/id tables) are regenerated.

mod read;
pub mod write;

pub use read::Image;

pub const MAGIC: u32 = 0x7371_7368;
pub const COMPRESSION_LZO: u16 = 3;
pub const METADATA_SIZE: usize = 8192;
pub const NO_FRAGMENT: u32 = 0xffff_ffff;
pub const NO_XATTR: u32 = 0xffff_ffff;
pub const NO_TABLE: u64 = u64::MAX;
/// Bit 24 of a data block size word: block stored uncompressed.
pub const BLOCK_UNCOMPRESSED: u32 = 1 << 24;

pub mod flags {
    pub const UNCOMPRESSED_INODES: u16 = 0x0001;
    pub const UNCOMPRESSED_DATA: u16 = 0x0002;
    pub const UNCOMPRESSED_FRAGMENTS: u16 = 0x0008;
    pub const NO_FRAGMENTS: u16 = 0x0010;
    pub const EXPORTABLE: u16 = 0x0080;
    pub const UNCOMPRESSED_XATTRS: u16 = 0x0100;
    pub const NO_XATTRS: u16 = 0x0200;
    pub const COMPRESSOR_OPTIONS: u16 = 0x0400;
    pub const UNCOMPRESSED_IDS: u16 = 0x0800;
}

#[derive(Debug, Clone)]
pub struct Superblock {
    pub inode_count: u32,
    pub mkfs_time: u32,
    pub block_size: u32,
    pub fragment_count: u32,
    pub compression: u16,
    pub block_log: u16,
    pub flags: u16,
    pub id_count: u16,
    pub root_inode: u64,
    pub bytes_used: u64,
    pub id_table: u64,
    pub xattr_table: u64,
    pub inode_table: u64,
    pub directory_table: u64,
    pub fragment_table: u64,
    pub export_table: u64,
}

impl Superblock {
    pub const SIZE: usize = 96;

    pub fn to_bytes(&self) -> [u8; 96] {
        let mut b = [0u8; 96];
        let mut w = Cursor::new(&mut b);
        w.u32(MAGIC);
        w.u32(self.inode_count);
        w.u32(self.mkfs_time);
        w.u32(self.block_size);
        w.u32(self.fragment_count);
        w.u16(self.compression);
        w.u16(self.block_log);
        w.u16(self.flags);
        w.u16(self.id_count);
        w.u16(4);
        w.u16(0);
        for v in [
            self.root_inode,
            self.bytes_used,
            self.id_table,
            self.xattr_table,
            self.inode_table,
            self.directory_table,
            self.fragment_table,
            self.export_table,
        ] {
            w.u64(v);
        }
        b
    }
}

/// Small little-endian writer over a fixed buffer.
struct Cursor<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }
    fn put(&mut self, b: &[u8]) {
        self.buf[self.pos..self.pos + b.len()].copy_from_slice(b);
        self.pos += b.len();
    }
    fn u16(&mut self, v: u16) {
        self.put(&v.to_le_bytes())
    }
    fn u32(&mut self, v: u32) {
        self.put(&v.to_le_bytes())
    }
    fn u64(&mut self, v: u64) {
        self.put(&v.to_le_bytes())
    }
}

/// Reference to an inode: metadata block offset (relative to the inode table) and
/// offset inside the uncompressed block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InodeRef {
    pub block: u32,
    pub offset: u16,
}

impl InodeRef {
    pub fn from_u64(v: u64) -> Self {
        InodeRef {
            block: (v >> 16) as u32,
            offset: (v & 0xffff) as u16,
        }
    }
    pub fn to_u64(self) -> u64 {
        ((self.block as u64) << 16) | self.offset as u64
    }
}

/// Where a regular file's data lives in the original image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLayout {
    pub blocks_start: u64,
    pub block_sizes: Vec<u32>,
    pub fragment: u32,
    pub fragment_offset: u32,
    pub file_size: u64,
    pub sparse: u64,
}

impl FileLayout {
    /// Compressed length of all data blocks.
    pub fn blocks_len(&self) -> u64 {
        self.block_sizes.iter().map(|&s| (s & !BLOCK_UNCOMPRESSED) as u64).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Dir {
        children: Vec<(Vec<u8>, usize)>,
    },
    File {
        layout: Option<FileLayout>,
        new_content: Option<Vec<u8>>,
    },
    Symlink {
        target: Vec<u8>,
    },
}

/// One inode of the file system tree. Hard links share one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: Kind,
    pub permissions: u16,
    pub uid_index: u16,
    pub gid_index: u16,
    pub mtime: u32,
    pub inode_number: u32,
    pub nlink: u32,
    /// The original inode used the extended format.
    pub extended: bool,
}

impl Node {
    pub fn type_name(&self) -> &'static str {
        match self.kind {
            Kind::Dir { .. } => "directory",
            Kind::File { .. } => "file",
            Kind::Symlink { .. } => "symlink",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fragment {
    pub start: u64,
    pub size: u32,
}

/// The file system tree of an image, plus what is needed to write it back.
#[derive(Debug, Clone)]
pub struct Fs {
    pub sb: Superblock,
    /// Compressor options metadata block, copied verbatim (header included).
    pub compressor_options: Vec<u8>,
    pub ids: Vec<u32>,
    pub fragments: Vec<Fragment>,
    pub nodes: Vec<Node>,
    pub root: usize,
}

impl Fs {
    /// Looks up a `/`-separated path relative to the root.
    pub fn lookup(&self, path: &str) -> Option<usize> {
        let mut cur = self.root;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            let Kind::Dir { children } = &self.nodes[cur].kind else {
                return None;
            };
            cur = children.iter().find(|(n, _)| n == part.as_bytes())?.1;
        }
        Some(cur)
    }

    /// All paths in depth-first order (root is ""), each with its node.
    pub fn walk(&self) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        let mut stack = vec![(String::new(), self.root)];
        while let Some((path, n)) = stack.pop() {
            if let Kind::Dir { children } = &self.nodes[n].kind {
                for (name, c) in children.iter().rev() {
                    let name = String::from_utf8_lossy(name);
                    stack.push((
                        if path.is_empty() {
                            name.to_string()
                        } else {
                            format!("{path}/{name}")
                        },
                        *c,
                    ));
                }
            }
            out.push((path, n));
        }
        out
    }

    /// Replaces the content of an existing regular file.
    pub fn set_content(&mut self, node: usize, data: Vec<u8>) -> crate::Result<()> {
        match &mut self.nodes[node].kind {
            Kind::File { new_content, .. } => {
                *new_content = Some(data);
                Ok(())
            }
            _ => Err(crate::err!("not a regular file")),
        }
    }

    /// Adds a new regular file `name` to directory `parent`, keeping entries sorted.
    pub fn add_file(&mut self, parent: usize, name: &str, permissions: u16, data: Vec<u8>) -> crate::Result<usize> {
        if name.is_empty() || name.contains('/') || name.len() > 256 {
            return Err(crate::err!("invalid file name '{name}'"));
        }
        let inode_number = self.nodes.iter().map(|n| n.inode_number).max().unwrap_or(0) + 1;
        let template = &self.nodes[self.root];
        let node = Node {
            kind: Kind::File {
                layout: None,
                new_content: Some(data),
            },
            permissions,
            uid_index: template.uid_index,
            gid_index: template.gid_index,
            mtime: self.sb.mkfs_time,
            inode_number,
            nlink: 1,
            extended: false,
        };
        let id = self.nodes.len();
        let Kind::Dir { children } = &mut self.nodes[parent].kind else {
            return Err(crate::err!("parent is not a directory"));
        };
        let pos = match children.binary_search_by(|(n, _)| n.as_slice().cmp(name.as_bytes())) {
            Ok(_) => return Err(crate::err!("'{name}' already exists")),
            Err(p) => p,
        };
        children.insert(pos, (name.as_bytes().to_vec(), id));
        self.nodes.push(node);
        Ok(id)
    }
}
