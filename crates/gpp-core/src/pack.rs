//! The `GEMINI_PACK.BIN` update container (SunPlus/GEMINI ISP update image).
//!
//! Layout as used by the head unit's U-Boot USB updater:
//! * `0x000`: header, magic `GEMINI`. At `0xd8` a little-endian length and at `0xdc`
//!   an offset of the region covered by the ASCII MD5 at `0xea` (tag `MD5SUM` at `0xe0`).
//! * `0x400` (the "image header offset"): the ISP image. At `+0x20` a U-Boot script
//!   uImage (the init script) that points to the main script uImage.
//! * The main script writes every partition in chunks
//!   (`fatload ... <len> $isp_update_file_offset`, the offset being set just before
//!   with `isp_image_header_offset + <off>`) and checks each chunk after writing it
//!   with `md5sum ... <len>` / `if test "$md5sum_value" = <md5>`.
//!
//! Replacing a partition therefore means: write the new bytes into its chunks,
//! update the chunk MD5s inside the script text (same length, so nothing moves),
//! then recompute the script uImage CRCs and the header MD5.

use crate::engine::Check;
use crate::{err, md5_hex, Result};

const BASE: usize = 0x400;
const UIMAGE_MAGIC: [u8; 4] = [0x27, 0x05, 0x19, 0x56];

#[derive(Debug, Clone)]
pub struct Layout {
    /// Absolute offset of the main script uImage header.
    pub script_header: usize,
    /// Absolute range of the script text (inside the uImage payload).
    pub script_text: (usize, usize),
    pub partitions: Vec<Partition>,
}

#[derive(Debug, Clone)]
pub struct Partition {
    pub name: String,
    pub chunks: Vec<Chunk>,
}

#[derive(Debug, Clone)]
pub struct Chunk {
    /// Absolute offset of the chunk data in the pack.
    pub offset: usize,
    pub len: usize,
    /// Number of bytes the script hashes after writing the chunk.
    pub md5_len: usize,
    pub md5: String,
    /// Absolute offset of the 32 hex digits of `md5` inside the script text.
    pub md5_pos: usize,
}

impl Partition {
    pub fn size(&self) -> usize {
        self.chunks.iter().map(|c| c.len).sum()
    }
}

fn u32le(d: &[u8], o: usize) -> Result<usize> {
    let b = d.get(o..o + 4).ok_or_else(|| err!("pack truncated at {o:#x}"))?;
    Ok(u32::from_le_bytes(b.try_into().unwrap()) as usize)
}

fn u32be(d: &[u8], o: usize) -> Result<usize> {
    let b = d.get(o..o + 4).ok_or_else(|| err!("pack truncated at {o:#x}"))?;
    Ok(u32::from_be_bytes(b.try_into().unwrap()) as usize)
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Parses `0x1234` at the start of `s`.
fn hex_prefix(s: &str) -> Option<usize> {
    let s = s.strip_prefix("0x")?;
    let end = s.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(s.len());
    usize::from_str_radix(&s[..end], 16).ok()
}

/// Value after the first `isp_image_header_offset + ` in `line`.
fn header_offset_arg(line: &str) -> Option<usize> {
    let i = line.find("isp_image_header_offset + ")?;
    hex_prefix(&line[i + "isp_image_header_offset + ".len()..])
}

fn uimage_size(d: &[u8], at: usize) -> Result<usize> {
    if d.get(at..at + 4) != Some(&UIMAGE_MAGIC[..]) {
        return Err(err!("no U-Boot script image at {at:#x}"));
    }
    u32be(d, at + 12)
}

impl Layout {
    pub fn parse(d: &[u8]) -> Result<Layout> {
        if !d.starts_with(b"GEMINI") {
            return Err(err!("not a GEMINI_PACK file (bad magic)"));
        }
        let init_at = BASE + 0x20;
        let init_size = uimage_size(d, init_at)?;
        let init_end = (init_at + 64 + init_size).min(d.len());
        let i = find(&d[..init_end], b"if test ", BASE).ok_or_else(|| err!("init script not found"))?;
        let init = String::from_utf8_lossy(&d[i..init_end]);
        let script_header = BASE + header_offset_arg(&init).ok_or_else(|| err!("main script offset not found"))?;
        let size = uimage_size(d, script_header)?;
        let payload_end = script_header + 64 + size;
        if payload_end > d.len() {
            return Err(err!("main script runs past end of file"));
        }
        // Script uImages carry a (length, 0) table before the text.
        let text_start = script_header + 0x48;
        let text_end = find(&d[..payload_end], b"\0", text_start).unwrap_or(payload_end);
        let text = std::str::from_utf8(&d[text_start..text_end]).map_err(|_| err!("main script is not text"))?;

        let mut partitions: Vec<Partition> = Vec::new();
        let mut pending_offset = None;
        let mut pending: Vec<(usize, usize)> = Vec::new(); // chunks waiting for their md5
        let mut md5_len = None;
        let mut pos = 0;
        for line in text.split_inclusive('\n') {
            let line_start = text_start + pos;
            pos += line.len();
            let l = line.trim_end();
            if let Some(rest) = l.strip_prefix("echo Updating ") {
                if let Some(name) = rest.strip_suffix(" ...") {
                    partitions.push(Partition {
                        name: name.trim_end_matches('.').to_string(),
                        chunks: Vec::new(),
                    });
                    pending.clear();
                    continue;
                }
            }
            if let Some(off) = header_offset_arg(l) {
                pending_offset = Some(off);
            }
            if l.starts_with("fatload ") && l.ends_with(" $isp_update_file_offset") {
                let len = l.split_whitespace().rev().nth(1).and_then(hex_prefix);
                if let (Some(len), Some(off), Some(_)) = (len, pending_offset, partitions.last()) {
                    pending.push((BASE + off, len));
                }
            }
            if l.starts_with("md5sum $isp_ram_addr ") {
                md5_len = l.split_whitespace().nth(2).and_then(hex_prefix);
            }
            if let Some(i) = l.find("\"$md5sum_value\" = ") {
                let hex_at = i + "\"$md5sum_value\" = ".len();
                let md5 = l.get(hex_at..hex_at + 32).unwrap_or("");
                if md5.len() == 32 && md5.bytes().all(|b| b.is_ascii_hexdigit()) {
                    if let Some(p) = partitions.last_mut() {
                        if p.chunks.len() < pending.len() {
                            let (offset, len) = pending[p.chunks.len()];
                            p.chunks.push(Chunk {
                                offset,
                                len,
                                md5_len: md5_len.take().unwrap_or(len),
                                md5: md5.to_string(),
                                md5_pos: line_start + hex_at,
                            });
                        }
                    }
                }
            }
        }
        partitions.retain(|p| !p.chunks.is_empty());
        for p in &partitions {
            for c in &p.chunks {
                if c.offset + c.len.max(c.md5_len) > d.len() {
                    return Err(err!(
                        "partition {} chunk at {:#x} runs past end of file",
                        p.name,
                        c.offset
                    ));
                }
            }
        }
        Ok(Layout {
            script_header,
            script_text: (text_start, text_end),
            partitions,
        })
    }

    pub fn partition(&self, name: &str) -> Result<&Partition> {
        self.partitions
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| err!("partition '{name}' not found in pack"))
    }

    pub fn read(&self, d: &[u8], name: &str) -> Result<Vec<u8>> {
        let p = self.partition(name)?;
        let mut out = Vec::with_capacity(p.size());
        for c in &p.chunks {
            out.extend_from_slice(&d[c.offset..c.offset + c.len]);
        }
        Ok(out)
    }

    /// Writes `new` (exactly the partition's size) into the partition's chunks and
    /// updates the chunk MD5s in the script. Call [`Layout::fix_checksums`] afterwards.
    pub fn write(&self, d: &mut [u8], name: &str, new: &[u8]) -> Result<()> {
        let p = self.partition(name)?;
        if new.len() != p.size() {
            return Err(err!(
                "new {name} image is {} bytes, partition holds exactly {}",
                new.len(),
                p.size()
            ));
        }
        let mut pos = 0;
        for c in &p.chunks {
            d[c.offset..c.offset + c.len].copy_from_slice(&new[pos..pos + c.len]);
            pos += c.len;
            let md5 = md5_hex(&d[c.offset..c.offset + c.md5_len]);
            d[c.md5_pos..c.md5_pos + 32].copy_from_slice(md5.as_bytes());
        }
        Ok(())
    }

    /// Recomputes the main script uImage CRCs and the pack header MD5.
    pub fn fix_checksums(&self, d: &mut [u8]) -> Result<()> {
        let h = self.script_header;
        let size = u32be(d, h + 12)?;
        let dcrc = crc32fast::hash(&d[h + 64..h + 64 + size]);
        d[h + 24..h + 28].copy_from_slice(&dcrc.to_be_bytes());
        d[h + 4..h + 8].fill(0);
        let hcrc = crc32fast::hash(&d[h..h + 64]);
        d[h + 4..h + 8].copy_from_slice(&hcrc.to_be_bytes());
        let (len, off) = header_md5_range(d)?;
        let md5 = md5_hex(&d[off..off + len]);
        d[0xea..0xea + 32].copy_from_slice(md5.as_bytes());
        Ok(())
    }
}

fn header_md5_range(d: &[u8]) -> Result<(usize, usize)> {
    if d.get(0xe0..0xe6) != Some(&b"MD5SUM"[..]) {
        return Err(err!("pack header has no MD5SUM field"));
    }
    let (len, off) = (u32le(d, 0xd8)?, u32le(d, 0xdc)?);
    if off.checked_add(len).is_none_or(|e| e > d.len()) {
        return Err(err!("pack header MD5 range runs past end of file"));
    }
    Ok((len, off))
}

fn uimage_ok(d: &[u8], at: usize) -> Result<bool> {
    let size = uimage_size(d, at)?;
    let data = d
        .get(at + 64..at + 64 + size)
        .ok_or_else(|| err!("script image truncated"))?;
    let mut h = d[at..at + 64].to_vec();
    let hcrc = u32::from_be_bytes(h[4..8].try_into().unwrap());
    let dcrc = u32::from_be_bytes(h[24..28].try_into().unwrap());
    h[4..8].fill(0);
    Ok(crc32fast::hash(&h) == hcrc && crc32fast::hash(data) == dcrc)
}

/// Every integrity check the updater performs on a pack.
pub fn validate(d: &[u8]) -> Vec<Check> {
    let mut checks = Vec::new();
    let layout = match Layout::parse(d) {
        Ok(l) => l,
        Err(e) => {
            checks.push(Check::fail("Pack structure", e.0));
            return checks;
        }
    };
    checks.push(match header_md5_range(d) {
        Ok((len, off)) => {
            let want = String::from_utf8_lossy(&d[0xea..0xea + 32]).to_string();
            let got = md5_hex(&d[off..off + len]);
            Check::new(
                "Pack header MD5",
                want == got,
                if want == got { want } else { format!("{got} != {want}") },
            )
        }
        Err(e) => Check::fail("Pack header MD5", e.0),
    });
    for (label, at) in [
        ("Init script CRC", BASE + 0x20),
        ("Update script CRC", layout.script_header),
    ] {
        checks.push(match uimage_ok(d, at) {
            Ok(ok) => Check::new(label, ok, if ok { "OK" } else { "CRC mismatch" }),
            Err(e) => Check::fail(label, e.0),
        });
    }
    let mut total = 0;
    let mut bad = Vec::new();
    for p in &layout.partitions {
        for c in &p.chunks {
            total += 1;
            if md5_hex(&d[c.offset..c.offset + c.md5_len]) != c.md5 {
                bad.push(format!("{}@{:#x}", p.name, c.offset));
            }
        }
    }
    checks.push(Check::new(
        "Partition chunk MD5s",
        bad.is_empty() && total > 0,
        if bad.is_empty() {
            format!("{total}/{total} OK")
        } else {
            format!("bad: {}", bad.join(", "))
        },
    ));
    checks
}
