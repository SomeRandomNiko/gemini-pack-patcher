//! LZO1X support.
//!
//! Decompression is implemented here, strictly: like the Linux kernel's
//! `lzo1x_decompress_safe` (which the head unit uses to read SquashFS) a stream is
//! rejected if it overruns, references data before the start of the output, or
//! leaves input unconsumed. Compression uses `lzokay-native` (an optimal-parsing
//! LZO1X compressor, close to `lzo1x_999` in ratio), and every compressed buffer is
//! round-tripped through the strict decompressor before it is used.

use crate::{err, Result};

/// Decompresses one LZO1X stream that must produce at most `max_out` bytes.
pub fn decompress(src: &[u8], max_out: usize) -> Result<Vec<u8>> {
    Ok(Decoder {
        src,
        ip: 0,
        out: Vec::with_capacity(max_out),
        max_out,
        sources: None,
    }
    .run()?
    .0)
}

/// Rewrites the literal bytes of an existing LZO1X `stream` so that it decompresses
/// to `target` (same length as the original output). The stream keeps its size, so
/// a small binary patch does not grow the image. Returns `None` if a changed byte was
/// encoded as part of a match, or if the result does not decompress to `target`
/// exactly (e.g. a later match copies from a patched byte).
pub fn patch_literals(stream: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    let d = Decoder {
        src: stream,
        ip: 0,
        out: Vec::with_capacity(target.len()),
        max_out: target.len(),
        sources: Some(Vec::new()),
    };
    let (out, sources) = d.run().ok()?;
    if out.len() != target.len() {
        return None;
    }
    let sources = sources?;
    let mut patched = stream.to_vec();
    for (i, (&a, &b)) in out.iter().zip(target).enumerate() {
        if a != b {
            let src = sources[i].checked_sub(1)? as usize;
            patched[src] = b;
        }
    }
    (decompress(&patched, target.len()).ok()? == target).then_some(patched)
}

/// Compresses `data`, verifying that the result decompresses back to `data`.
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let c = lzokay_native::compress(data).map_err(|e| err!("LZO compression failed: {e}"))?;
    if c.first() == Some(&17) {
        // Newer kernels treat a leading 17 as an LZO-RLE version marker.
        return Err(err!("LZO compressor produced an ambiguous stream start"));
    }
    match decompress(&c, data.len()) {
        Ok(d) if d == data => Ok(c),
        _ => Err(err!("LZO round-trip check failed")),
    }
}

enum State {
    Outer,
    FirstLiteralRun,
    Match(usize),
    MatchDone,
}

struct Decoder<'a> {
    src: &'a [u8],
    ip: usize,
    out: Vec<u8>,
    max_out: usize,
    /// Per output byte: input position + 1 if it was a literal, 0 if it came from a match.
    sources: Option<Vec<u32>>,
}

impl Decoder<'_> {
    fn byte(&mut self) -> Result<usize> {
        let b = *self.src.get(self.ip).ok_or_else(|| err!("LZO: input overrun"))?;
        self.ip += 1;
        Ok(b as usize)
    }

    fn le16(&mut self) -> Result<usize> {
        Ok(self.byte()? | (self.byte()? << 8))
    }

    /// Run-length extension used by long literal runs and matches.
    fn extended(&mut self, base: usize) -> Result<usize> {
        let mut n = 0;
        loop {
            let b = self.byte()?;
            if b != 0 {
                return Ok(n + base + b);
            }
            n += 255;
            if n > self.max_out + 255 {
                return Err(err!("LZO: run length overflow"));
            }
        }
    }

    fn literals(&mut self, n: usize) -> Result<()> {
        let end = self
            .ip
            .checked_add(n)
            .filter(|&e| e <= self.src.len())
            .ok_or_else(|| err!("LZO: input overrun"))?;
        if self.out.len() + n > self.max_out {
            return Err(err!("LZO: output overrun"));
        }
        self.out.extend_from_slice(&self.src[self.ip..end]);
        if let Some(s) = &mut self.sources {
            s.extend((self.ip..end).map(|p| p as u32 + 1));
        }
        self.ip = end;
        Ok(())
    }

    fn copy_match(&mut self, dist: usize, len: usize) -> Result<()> {
        if dist == 0 || dist > self.out.len() {
            return Err(err!("LZO: lookbehind overrun"));
        }
        if self.out.len() + len > self.max_out {
            return Err(err!("LZO: output overrun"));
        }
        let start = self.out.len() - dist;
        for i in 0..len {
            let b = self.out[start + i];
            self.out.push(b);
        }
        if let Some(s) = &mut self.sources {
            s.resize(s.len() + len, 0);
        }
        Ok(())
    }

    fn run(mut self) -> Result<(Vec<u8>, Option<Vec<u32>>)> {
        let mut state = State::Outer;
        if let Some(&first) = self.src.first() {
            if first > 17 {
                self.ip = 1;
                let t = first as usize - 17;
                self.literals(t)?;
                state = if t < 4 {
                    State::Match(self.byte()?)
                } else {
                    State::FirstLiteralRun
                };
            }
        }
        loop {
            state = match state {
                State::Outer => {
                    let t = self.byte()?;
                    if t >= 16 {
                        State::Match(t)
                    } else {
                        let n = if t == 0 { self.extended(15)? } else { t };
                        self.literals(n + 3)?;
                        State::FirstLiteralRun
                    }
                }
                State::FirstLiteralRun => {
                    let t = self.byte()?;
                    if t >= 16 {
                        State::Match(t)
                    } else {
                        let dist = 1 + 0x800 + (t >> 2) + (self.byte()? << 2);
                        self.copy_match(dist, 3)?;
                        State::MatchDone
                    }
                }
                State::Match(t) => {
                    if t >= 64 {
                        let dist = 1 + ((t >> 2) & 7) + (self.byte()? << 3);
                        self.copy_match(dist, (t >> 5) + 1)?;
                    } else if t >= 32 {
                        let len = if t & 31 == 0 { self.extended(31)? } else { t & 31 };
                        let dist = 1 + (self.le16()? >> 2);
                        self.copy_match(dist, len + 2)?;
                    } else if t >= 16 {
                        let len = if t & 7 == 0 { self.extended(7)? } else { t & 7 };
                        let dist = ((t & 8) << 11) + (self.le16()? >> 2);
                        if dist == 0 {
                            if len != 1 {
                                return Err(err!("LZO: malformed end-of-stream marker"));
                            }
                            if self.ip != self.src.len() {
                                return Err(err!("LZO: input not fully consumed"));
                            }
                            return Ok((self.out, self.sources));
                        }
                        self.copy_match(dist + 0x4000, len + 2)?;
                    } else {
                        let dist = 1 + (t >> 2) + (self.byte()? << 2);
                        self.copy_match(dist, 2)?;
                    }
                    State::MatchDone
                }
                State::MatchDone => {
                    let t = (self.src[self.ip - 2] & 3) as usize;
                    if t == 0 {
                        State::Outer
                    } else {
                        self.literals(t)?;
                        State::Match(self.byte()?)
                    }
                }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut data = Vec::new();
        for i in 0..40000u32 {
            data.extend_from_slice(format!("line {} {}\n", i % 97, i * 7 % 13).as_bytes());
        }
        data.truncate(32768);
        let c = compress(&data).unwrap();
        assert!(c.len() < data.len() / 2);
        assert_eq!(decompress(&c, data.len()).unwrap(), data);
        assert!(decompress(&c, data.len() - 1).is_err());
        let mut trailing = c.clone();
        trailing.push(0);
        assert!(decompress(&trailing, data.len()).is_err());
    }

    #[test]
    fn literal_patching() {
        let mut x = 7u32;
        let data: Vec<u8> = (0..4000)
            .map(|i| {
                x = x.wrapping_mul(1103515245).wrapping_add(12345);
                if i % 3 == 0 {
                    (x >> 16) as u8
                } else {
                    b'a'
                }
            })
            .collect();
        let c = compress(&data).unwrap();
        let (out, sources) = Decoder {
            src: &c,
            ip: 0,
            out: Vec::new(),
            max_out: data.len(),
            sources: Some(Vec::new()),
        }
        .run()
        .unwrap();
        assert_eq!(out, data);
        let sources = sources.unwrap();
        let lit = sources.iter().position(|&s| s != 0).unwrap();
        let mut target = data.clone();
        target[lit] ^= 0x55;
        if let Some(p) = patch_literals(&c, &target) {
            assert_eq!(p.len(), c.len());
            assert_eq!(decompress(&p, target.len()).unwrap(), target);
        }
        let in_match = sources.iter().position(|&s| s == 0).unwrap();
        let mut target = data.clone();
        target[in_match] ^= 0x55;
        assert!(patch_literals(&c, &target).is_none());
    }

    #[test]
    fn incompressible_and_tiny() {
        let mut x = 0x12345678u32;
        let data: Vec<u8> = (0..5000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        for len in [1, 2, 3, 4, 5, 17, 300, 5000] {
            let c = compress(&data[..len]).unwrap();
            assert_eq!(decompress(&c, len).unwrap(), &data[..len]);
        }
    }
}
