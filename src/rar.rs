// Read RAR archives (RAR4 and RAR5) on TorBox's CDN with range requests.
//
// Like ZIPs, NSP/NSZ/XCI files are usually RARed with "store" (no compression),
// so a file's bytes sit unchanged in the archive, possibly split across volumes
// (.part1.rar, .part2.rar, ... or .rar, .r00, .r01, ...). It can be served as
// the list of those byte ranges. RAR has no central directory, so each volume's
// headers are walked one by one, skipping over the file data between them.
use crate::cache::Part;
use crate::sources::Error;
use crate::torbox::{cdn_range, is_switch_file};
use reqwest::Client;
use std::collections::HashMap;

const RAR5_SIGNATURE: &[u8] = b"Rar!\x1a\x07\x01\x00";
const RAR4_SIGNATURE: &[u8] = b"Rar!\x1a\x07\x00";
const HEADER_READ: u64 = 4096;
const MAX_HEADERS: usize = 20_000;

/// A stored file's bytes within one volume
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub name: String,
    /// Where the bytes start in the volume, and how many there are
    pub offset: u64,
    pub len: u64,
    /// Size of the whole file, across all volumes
    pub size: u64,
    /// Split flags: the file started in an earlier volume / goes on in a later one
    pub continued: bool,
    pub continues: bool,
}

/// A stored file, assembled from its pieces across volumes
#[derive(Debug, Clone, PartialEq)]
pub struct RarEntry {
    pub name: String,
    pub size: u64,
    pub parts: Vec<Part>,
}

#[derive(Debug, PartialEq)]
enum Block {
    File(FileHeader),
    End,
    /// Headers are encrypted with a password; nothing can be read
    Encrypted,
    Other,
}

#[derive(Debug, PartialEq)]
struct FileHeader {
    name: String,
    size: u64,
    stored: bool,
    solid: bool,
    encrypted: bool,
    directory: bool,
    continued: bool,
    continues: bool,
}

#[derive(Debug, PartialEq)]
struct Header {
    block: Block,
    /// Length of the header itself, and of the data that follows it
    header_len: u64,
    data_len: u64,
}

pub fn is_rar_volume(name: &str) -> bool {
    volume_key(name).is_some()
}

/// Sort key for volumes: (set name, position). "x.part2.rar" comes after
/// "x.part1.rar"; old-style sets go "x.rar", "x.r00", "x.r01".
pub fn volume_key(name: &str) -> Option<(String, i64)> {
    let lower = name.to_lowercase();
    if let Some(stem) = lower.strip_suffix(".rar") {
        if let Some((set, number)) = stem.rsplit_once(".part") {
            if let Ok(number) = number.parse::<i64>() {
                return Some((set.to_string(), number));
            }
        }
        return Some((stem.to_string(), -1));
    }
    let (stem, ext) = lower.rsplit_once('.')?;
    let number = ext.strip_prefix('r')?;
    if number.len() >= 2 && number.chars().all(|c| c.is_ascii_digit()) {
        return Some((stem.to_string(), number.parse().ok()?));
    }
    None
}

/// The stored Switch files' pieces in the volume at `url`, which is `size` bytes long.
pub async fn read_pieces(client: &Client, url: &str, size: u64) -> Result<Vec<Piece>, Error> {
    let start = get_range(client, url, 0, HEADER_READ.min(size) - 1).await?;
    let (rar5, mut pos) = if start.starts_with(RAR5_SIGNATURE) {
        (true, RAR5_SIGNATURE.len() as u64)
    } else if start.starts_with(RAR4_SIGNATURE) {
        (false, RAR4_SIGNATURE.len() as u64)
    } else {
        return Err("not a RAR file".into());
    };

    let mut pieces = Vec::new();
    for _ in 0..MAX_HEADERS {
        if pos >= size {
            break;
        }
        let mut buf = get_range(client, url, pos, (pos + HEADER_READ).min(size) - 1).await?;
        let needed = header_len(&buf, rar5).ok_or("truncated RAR header")?;
        if needed > buf.len() as u64 {
            buf = get_range(client, url, pos, (pos + needed).min(size) - 1).await?;
        }
        let header = parse_header(&buf, rar5).ok_or("bad RAR header")?;

        match header.block {
            Block::End => break,
            Block::Encrypted => return Err("RAR headers are password-protected".into()),
            Block::File(file) => {
                if file.stored && !file.solid && !file.encrypted && !file.directory && is_switch_file(&file.name) {
                    pieces.push(Piece {
                        name: file.name,
                        offset: pos + header.header_len,
                        len: header.data_len,
                        size: file.size,
                        continued: file.continued,
                        continues: file.continues,
                    });
                }
            }
            Block::Other => {}
        }
        pos += header.header_len + header.data_len;
    }

    Ok(pieces)
}

/// Join files split across volumes. `volumes` must be in volume order. Files
/// with a missing piece (or whose pieces don't add up) are dropped.
pub fn assemble(volumes: &[(String, Vec<Piece>)]) -> Vec<RarEntry> {
    let mut entries = Vec::new();
    let mut open: HashMap<String, RarEntry> = HashMap::new();

    for (volume, pieces) in volumes {
        for piece in pieces {
            let part = Part { volume: volume.clone(), offset: piece.offset, len: piece.len };
            if piece.continued {
                match open.get_mut(&piece.name) {
                    Some(entry) => entry.parts.push(part),
                    None => continue, // the start of this file is missing
                }
            } else {
                open.insert(piece.name.clone(), RarEntry { name: piece.name.clone(), size: piece.size, parts: vec![part] });
            }

            if !piece.continues {
                if let Some(entry) = open.remove(&piece.name) {
                    if entry.parts.iter().map(|p| p.len).sum::<u64>() == entry.size {
                        entries.push(entry);
                    }
                }
            }
        }
    }

    entries
}

async fn get_range(client: &Client, url: &str, start: u64, end: u64) -> Result<Vec<u8>, Error> {
    Ok(cdn_range(client, url, start, end).await?.bytes().await?.to_vec())
}

/// How many bytes the header at the start of `buf` takes, from its first few bytes.
fn header_len(buf: &[u8], rar5: bool) -> Option<u64> {
    if rar5 {
        let (size, after) = vint(buf, 4)?;
        Some(after as u64 + size)
    } else {
        Some(u16_at(buf, 5)? as u64)
    }
}

fn parse_header(buf: &[u8], rar5: bool) -> Option<Header> {
    if rar5 { parse_rar5(buf) } else { parse_rar4(buf) }
}

/// RAR5 block: CRC32, header size, type, flags, [extra size], [data size], type-specific fields.
fn parse_rar5(buf: &[u8]) -> Option<Header> {
    let (size, body) = vint(buf, 4)?;
    let header_len = body as u64 + size;
    let header = buf.get(..header_len as usize)?;

    let (kind, i) = vint(header, body)?;
    let (flags, mut i) = vint(header, i)?;
    let mut extra_len = 0;
    if flags & 0x01 != 0 {
        (extra_len, i) = vint(header, i)?;
    }
    let mut data_len = 0;
    if flags & 0x02 != 0 {
        (data_len, i) = vint(header, i)?;
    }

    let block = match kind {
        2 => {
            let (file_flags, j) = vint(header, i)?;
            let (unpacked, j) = vint(header, j)?;
            let (_attributes, mut j) = vint(header, j)?;
            if file_flags & 0x02 != 0 {
                j += 4; // modification time
            }
            if file_flags & 0x04 != 0 {
                j += 4; // CRC32
            }
            let (compression, j) = vint(header, j)?;
            let (_host_os, j) = vint(header, j)?;
            let (name_len, j) = vint(header, j)?;
            let name = header.get(j..j + name_len as usize)?;
            // Encryption is recorded as an extra area record of type 1, at the end of the header
            let extra = header.get(header.len().checked_sub(extra_len as usize)?..)?;
            Block::File(FileHeader {
                name: String::from_utf8_lossy(name).into_owned(),
                size: unpacked,
                stored: (compression >> 7) & 0x07 == 0,
                solid: compression & 0x40 != 0,
                encrypted: has_rar5_record(extra, 0x01),
                directory: file_flags & 0x01 != 0,
                continued: flags & 0x08 != 0,
                continues: flags & 0x10 != 0,
            })
        }
        4 => Block::Encrypted,
        5 => Block::End,
        _ => Block::Other,
    };

    Some(Header { block, header_len, data_len })
}

fn has_rar5_record(extra: &[u8], wanted: u64) -> bool {
    let mut pos = 0;
    while let Some((size, after_size)) = vint(extra, pos) {
        let Some((kind, _)) = vint(extra, after_size) else { break };
        if kind == wanted {
            return true;
        }
        pos = after_size + size as usize;
    }
    false
}

/// RAR4 block: CRC16, type, flags, header size, [added size], type-specific fields.
fn parse_rar4(buf: &[u8]) -> Option<Header> {
    let kind = *buf.get(2)?;
    let flags = u16_at(buf, 3)?;
    let header_len = u16_at(buf, 5)? as u64;
    let added = if flags & 0x8000 != 0 { u32_at(buf, 7)? as u64 } else { 0 };

    let (block, data_len) = match kind {
        // Archive header: 0x80 means the block headers are encrypted
        0x73 if flags & 0x0080 != 0 => (Block::Encrypted, 0),
        0x74 => {
            let mut packed = u32_at(buf, 7)? as u64;
            let mut size = u32_at(buf, 11)? as u64;
            let method = *buf.get(25)?;
            let name_len = u16_at(buf, 26)? as usize;
            let mut name_start = 32;
            if flags & 0x0100 != 0 {
                packed |= (u32_at(buf, 32)? as u64) << 32;
                size |= (u32_at(buf, 36)? as u64) << 32;
                name_start = 40;
            }
            let name = buf.get(name_start..name_start + name_len)?;
            // Unicode names put the encoded form after a NUL; the part before it is enough
            let name = name.split(|&b| b == 0).next().unwrap_or_default();
            let file = FileHeader {
                name: String::from_utf8_lossy(name).replace('\\', "/"),
                size,
                stored: method == 0x30,
                solid: flags & 0x0010 != 0,
                encrypted: flags & 0x0004 != 0,
                directory: flags & 0x00E0 == 0x00E0,
                continued: flags & 0x0001 != 0,
                continues: flags & 0x0002 != 0,
            };
            (Block::File(file), packed)
        }
        0x7B => (Block::End, 0),
        _ => (Block::Other, added),
    };

    Some(Header { block, header_len, data_len })
}

/// RAR5 variable-length integer at `at`: 7 bits per byte, high bit set on all but the last.
/// Returns the value and the position after it.
fn vint(buf: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (n, &byte) in buf.get(at..)?.iter().take(10).enumerate() {
        value |= ((byte & 0x7F) as u64) << (7 * n);
        if byte & 0x80 == 0 {
            return Some((value, at + n + 1));
        }
    }
    None
}

fn u16_at(buf: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buf.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_vint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    /// A RAR5 file header: (name, data length, unpacked size, header flags, compression info)
    fn rar5_file(name: &str, data_len: u64, size: u64, flags: u64, compression: u64) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(encode_vint(2)); // type: file
        body.extend(encode_vint(flags | 0x02)); // has data
        body.extend(encode_vint(data_len));
        body.extend(encode_vint(0x04)); // file flags: CRC present
        body.extend(encode_vint(size));
        body.extend(encode_vint(0x20)); // attributes
        body.extend([0xAA; 4]); // CRC32
        body.extend(encode_vint(compression));
        body.extend(encode_vint(1)); // host OS
        body.extend(encode_vint(name.len() as u64));
        body.extend(name.as_bytes());
        let mut header = vec![0; 4]; // CRC32 of the header, not checked
        header.extend(encode_vint(body.len() as u64));
        header.extend(body);
        header
    }

    #[test]
    fn parses_rar5_stored_file_headers() {
        let header = rar5_file("Game/Hades [01000D200AC0C000][v0].nsp", 5000, 5000, 0, 0);
        let parsed = parse_rar5(&header).unwrap();
        assert_eq!(parsed.header_len, header.len() as u64);
        assert_eq!(parsed.data_len, 5000);
        let Block::File(file) = parsed.block else { panic!("not a file header") };
        assert_eq!(file.name, "Game/Hades [01000D200AC0C000][v0].nsp");
        assert!(file.stored && !file.solid && !file.encrypted && !file.continued && !file.continues);
    }

    #[test]
    fn spots_rar5_compression_and_splits() {
        // Method 3 in bits 7-9, split into the next volume
        let header = rar5_file("big.nsp", 100, 300, 0x10, 3 << 7);
        let Block::File(file) = parse_rar5(&header).unwrap().block else { panic!() };
        assert!(!file.stored);
        assert!(file.continues && !file.continued);
    }

    #[test]
    fn parses_rar4_large_file_headers() {
        let name = b"Game\\Mario [0100000000010000][v0].nsp";
        let mut header = Vec::new();
        header.extend([0, 0, 0x74]); // CRC, type
        header.extend((0x8000u16 | 0x0100 | 0x0002).to_le_bytes()); // long block, large, split after
        header.extend(((40 + name.len()) as u16).to_le_bytes());
        header.extend(0x0000_0010u32.to_le_bytes()); // packed (low)
        header.extend(0x0000_0020u32.to_le_bytes()); // unpacked (low)
        header.push(2); // host OS
        header.extend([0; 8]); // CRC, time
        header.push(29); // version
        header.push(0x30); // method: store
        header.extend((name.len() as u16).to_le_bytes());
        header.extend([0; 4]); // attributes
        header.extend(1u32.to_le_bytes()); // packed (high)
        header.extend(2u32.to_le_bytes()); // unpacked (high)
        header.extend(name);

        let parsed = parse_rar4(&header).unwrap();
        assert_eq!(parsed.header_len, header.len() as u64);
        assert_eq!(parsed.data_len, (1u64 << 32) | 0x10);
        let Block::File(file) = parsed.block else { panic!() };
        assert_eq!(file.name, "Game/Mario [0100000000010000][v0].nsp");
        assert_eq!(file.size, (2u64 << 32) | 0x20);
        assert!(file.stored && file.continues && !file.continued);
    }

    #[test]
    fn orders_volumes() {
        let mut names = vec!["a.part10.rar", "a.part2.rar", "a.part1.rar", "b.r01", "b.rar", "b.r00", "readme.txt"];
        names.retain(|n| is_rar_volume(n));
        names.sort_by_key(|n| volume_key(n));
        assert_eq!(names, ["a.part1.rar", "a.part2.rar", "a.part10.rar", "b.rar", "b.r00", "b.r01"]);
    }

    #[test]
    fn assembles_split_files() {
        let piece = |name: &str, offset, len, continued, continues| Piece {
            name: name.into(), offset, len, size: 250, continued, continues,
        };
        let volumes = vec![
            ("x.part1.rar".to_string(), vec![piece("whole.nsp", 10, 250, false, false), piece("big.nsp", 300, 100, false, true)]),
            ("x.part2.rar".to_string(), vec![piece("big.nsp", 50, 100, true, true)]),
            ("x.part3.rar".to_string(), vec![piece("big.nsp", 50, 50, true, false), piece("orphan.nsp", 120, 250, true, false)]),
        ];

        let entries = assemble(&volumes);
        assert_eq!(entries.len(), 2, "the orphan continuation is dropped");
        assert_eq!(entries[0].name, "whole.nsp");
        assert_eq!(entries[1].name, "big.nsp");
        assert_eq!(entries[1].parts, vec![
            Part { volume: "x.part1.rar".into(), offset: 300, len: 100 },
            Part { volume: "x.part2.rar".into(), offset: 50, len: 100 },
            Part { volume: "x.part3.rar".into(), offset: 50, len: 50 },
        ]);
    }

    #[test]
    fn drops_files_with_missing_volumes() {
        let volumes = vec![("x.part1.rar".to_string(), vec![Piece {
            name: "big.nsp".into(), offset: 0, len: 100, size: 300, continued: false, continues: false,
        }])];
        assert!(assemble(&volumes).is_empty(), "pieces add up to 100 of 300 bytes");
    }
}
