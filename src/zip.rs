// Read ZIP archives on TorBox's CDN with range requests, without downloading them.
//
// NSP/NSZ/XCI files are encrypted, so uploaders usually ZIP them without
// compression ("stored"). A stored entry's bytes sit unchanged inside the ZIP,
// so it can be served to Tinfoil as a slice of the archive.
use crate::sources::Error;
use crate::torbox::{cdn_range, is_switch_file};
use reqwest::Client;

const END_OF_CENTRAL_DIR: &[u8] = b"PK\x05\x06";
const ZIP64_END_LOCATOR: &[u8] = b"PK\x06\x07";
const ZIP64_END: &[u8] = b"PK\x06\x06";
const CENTRAL_DIR_ENTRY: &[u8] = b"PK\x01\x02";
const LOCAL_HEADER: &[u8] = b"PK\x03\x04";
const METHOD_STORED: u16 = 0;
// The end record is at most 22 bytes plus a 64 KiB comment, with ZIP64 records before it
const TAIL_SIZE: u64 = 128 * 1024;

/// A stored NSP/NSZ/XCI inside a ZIP
#[derive(Debug, Clone, PartialEq)]
pub struct ZipEntry {
    pub name: String,
    pub size: u64,
    /// Offset of the entry's local header within the ZIP
    pub header_offset: u64,
}

/// List the stored Switch files in the ZIP at `url`, which is `zip_size` bytes long.
pub async fn read_entries(client: &Client, url: &str, zip_size: u64) -> Result<Vec<ZipEntry>, Error> {
    let tail_start = zip_size.saturating_sub(TAIL_SIZE);
    let tail = get_range(client, url, tail_start, zip_size - 1).await?;
    let (dir_offset, dir_size) = find_central_directory(&tail, tail_start)?;

    let directory = get_range(client, url, dir_offset, dir_offset + dir_size - 1).await?;
    Ok(parse_central_directory(&directory))
}

/// Where the data of the entry whose local header is at `header_offset` starts.
pub async fn data_offset(client: &Client, url: &str, header_offset: u64) -> Result<u64, Error> {
    let header = get_range(client, url, header_offset, header_offset + 29).await?;
    local_data_offset(&header, header_offset)
}

async fn get_range(client: &Client, url: &str, start: u64, end: u64) -> Result<Vec<u8>, Error> {
    Ok(cdn_range(client, url, start, end).await?.bytes().await?.to_vec())
}

/// Find the central directory from the last bytes of the file, which start at
/// `tail_start`. Returns (offset, size).
fn find_central_directory(tail: &[u8], tail_start: u64) -> Result<(u64, u64), Error> {
    let end = rfind(tail, END_OF_CENTRAL_DIR).ok_or("not a ZIP file (no end of central directory)")?;
    let record = tail.get(end..end + 22).ok_or("truncated ZIP end record")?;
    let size = u32_at(record, 12) as u64;
    let offset = u32_at(record, 16) as u64;

    if offset != u32::MAX as u64 && size != u32::MAX as u64 {
        return Ok((offset, size));
    }

    // ZIP64: the locator just before the end record points at the ZIP64 end record
    let locator = tail.get(end.checked_sub(20).ok_or("missing ZIP64 locator")?..end)
        .filter(|l| l.starts_with(ZIP64_END_LOCATOR))
        .ok_or("missing ZIP64 locator")?;
    let zip64_end = u64_at(locator, 8)
        .checked_sub(tail_start)
        .ok_or("ZIP64 end record is outside the bytes read")? as usize;
    let record = tail.get(zip64_end..zip64_end + 56)
        .filter(|r| r.starts_with(ZIP64_END))
        .ok_or("bad ZIP64 end record")?;
    Ok((u64_at(record, 48), u64_at(record, 40)))
}

/// Stored Switch files from a central directory. Compressed entries are skipped.
fn parse_central_directory(directory: &[u8]) -> Vec<ZipEntry> {
    let mut entries = Vec::new();
    let mut pos = 0;

    while let Some(header) = directory.get(pos..pos + 46).filter(|h| h.starts_with(CENTRAL_DIR_ENTRY)) {
        let method = u16_at(header, 10);
        let mut compressed = u32_at(header, 20) as u64;
        let mut size = u32_at(header, 24) as u64;
        let name_len = u16_at(header, 28) as usize;
        let extra_len = u16_at(header, 30) as usize;
        let comment_len = u16_at(header, 32) as usize;
        let mut header_offset = u32_at(header, 42) as u64;

        let name_start = pos + 46;
        let extra_start = name_start + name_len;
        let (Some(name), Some(extra)) = (
            directory.get(name_start..extra_start),
            directory.get(extra_start..extra_start + extra_len),
        ) else {
            break;
        };

        // ZIP64 extra field: only the values that overflowed 32 bits are present, in this order
        if let Some(mut values) = zip64_extra(extra) {
            for field in [&mut size, &mut compressed, &mut header_offset] {
                if *field == u32::MAX as u64 {
                    if let Some(value) = values.next() {
                        *field = value;
                    }
                }
            }
        }

        let name = String::from_utf8_lossy(name).into_owned();
        if method == METHOD_STORED && compressed == size && is_switch_file(&name) {
            entries.push(ZipEntry { name, size, header_offset });
        }

        pos = extra_start + extra_len + comment_len;
    }

    entries
}

fn zip64_extra(extra: &[u8]) -> Option<impl Iterator<Item = u64> + '_> {
    let mut pos = 0;
    while let Some(field) = extra.get(pos..pos + 4) {
        let (id, len) = (u16_at(field, 0), u16_at(field, 2) as usize);
        let data = extra.get(pos + 4..pos + 4 + len)?;
        if id == 0x0001 {
            return Some(data.chunks_exact(8).map(|c| u64_at(c, 0)));
        }
        pos += 4 + len;
    }
    None
}

/// Data starts after the 30-byte local header, its file name and its extra field
/// (which can differ from the central directory's).
fn local_data_offset(header: &[u8], header_offset: u64) -> Result<u64, Error> {
    let header = header.get(..30)
        .filter(|h| h.starts_with(LOCAL_HEADER))
        .ok_or("bad ZIP local header")?;
    let name_len = u16_at(header, 26) as u64;
    let extra_len = u16_at(header, 28) as u64;
    Ok(header_offset + 30 + name_len + extra_len)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a small ZIP: (name, data, method) for each entry.
    fn build_zip(files: &[(&str, &[u8], u16)]) -> Vec<u8> {
        let mut zip = Vec::new();
        let mut directory = Vec::new();
        for (name, data, method) in files {
            let offset = zip.len() as u32;
            let extra = b"\x99\x99\x02\x00ab"; // an unrelated extra field, as some zippers add
            zip.extend(LOCAL_HEADER);
            zip.extend([20, 0, 0, 0]);
            zip.extend(method.to_le_bytes());
            zip.extend([0; 8]); // time, date, crc
            zip.extend((data.len() as u32).to_le_bytes());
            zip.extend((data.len() as u32).to_le_bytes());
            zip.extend((name.len() as u16).to_le_bytes());
            zip.extend((extra.len() as u16).to_le_bytes());
            zip.extend(name.as_bytes());
            zip.extend(extra);
            zip.extend(*data);

            directory.extend(CENTRAL_DIR_ENTRY);
            directory.extend([20, 0, 20, 0, 0, 0]);
            directory.extend(method.to_le_bytes());
            directory.extend([0; 8]);
            directory.extend((data.len() as u32).to_le_bytes());
            directory.extend((data.len() as u32).to_le_bytes());
            directory.extend((name.len() as u16).to_le_bytes());
            directory.extend([0; 2]); // extra length
            directory.extend([0; 2]); // comment length
            directory.extend([0; 8]); // disk, attributes
            directory.extend(offset.to_le_bytes());
            directory.extend(name.as_bytes());
        }
        let dir_offset = zip.len() as u32;
        zip.extend(&directory);
        zip.extend(END_OF_CENTRAL_DIR);
        zip.extend([0; 4]);
        zip.extend((files.len() as u16).to_le_bytes());
        zip.extend((files.len() as u16).to_le_bytes());
        zip.extend((directory.len() as u32).to_le_bytes());
        zip.extend(dir_offset.to_le_bytes());
        zip.extend([0; 2]);
        zip
    }

    #[test]
    fn finds_stored_switch_files_and_their_bytes() {
        let zip = build_zip(&[
            ("Game/readme.txt", b"hello", 0),
            ("Game/Hades [01000D200AC0C000][v0].nsp", b"PFS0-hades-bytes", 0),
            ("Game/Packed [0100000000010000][v0].nsp", b"deflated", 8),
        ]);

        let (offset, size) = find_central_directory(&zip, 0).unwrap();
        let entries = parse_central_directory(&zip[offset as usize..(offset + size) as usize]);
        assert_eq!(entries.len(), 1, "only the stored NSP is usable");
        assert_eq!(entries[0].name, "Game/Hades [01000D200AC0C000][v0].nsp");

        let start = local_data_offset(&zip[entries[0].header_offset as usize..], entries[0].header_offset).unwrap();
        let data = &zip[start as usize..(start + entries[0].size) as usize];
        assert_eq!(data, b"PFS0-hades-bytes");
    }

    #[test]
    fn reads_zip64_sizes_and_offsets() {
        // Central directory entry with 32-bit fields maxed out and the real values in the ZIP64 extra
        let name = b"big.nsp";
        let mut entry = Vec::new();
        entry.extend(CENTRAL_DIR_ENTRY);
        entry.extend([45, 0, 45, 0, 0, 0, 0, 0]);
        entry.extend([0; 8]);
        entry.extend(u32::MAX.to_le_bytes());
        entry.extend(u32::MAX.to_le_bytes());
        entry.extend((name.len() as u16).to_le_bytes());
        entry.extend(28u16.to_le_bytes());
        entry.extend([0; 10]); // comment length, disk, attributes
        entry.extend(u32::MAX.to_le_bytes());
        entry.extend(name);
        entry.extend(1u16.to_le_bytes());
        entry.extend(24u16.to_le_bytes());
        entry.extend(14_000_000_000u64.to_le_bytes()); // size
        entry.extend(14_000_000_000u64.to_le_bytes()); // compressed size
        entry.extend(6_000_000_000u64.to_le_bytes()); // header offset

        let entries = parse_central_directory(&entry);
        assert_eq!(entries, vec![ZipEntry { name: "big.nsp".into(), size: 14_000_000_000, header_offset: 6_000_000_000 }]);
    }

    #[test]
    fn rejects_non_zips() {
        assert!(find_central_directory(b"definitely not a zip file", 0).is_err());
    }
}
