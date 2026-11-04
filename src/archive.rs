//! LoxCC header/LZ4 handling follows loxMqttRelay's validated container path.
//! ZIP decoding uses only deflate; decompression is bounded before allocation.
use anyhow::{Context, Result, bail, ensure};
use std::io::{Cursor, Read};

pub const MAX_CONFIG_BYTES: usize = 64 * 1024 * 1024;
const MAGIC: u32 = 0xaabbccee;

pub fn decode(raw: &[u8]) -> Result<(Vec<u8>, Option<serde_json::Value>)> {
    ensure!(
        raw.len() <= MAX_CONFIG_BYTES,
        "Configuration download exceeds size limit"
    );
    if raw.starts_with(b"PK") {
        let mut zip = zip::ZipArchive::new(Cursor::new(raw)).context("Invalid deployment ZIP")?;
        let xml = container(&entry(&mut zip, "sps0.LoxCC")?)?;
        let json = match zip.by_name("LoxAPP3.json") {
            Ok(file) => Some(serde_json::from_slice(&bounded_read(file)?)?),
            Err(zip::result::ZipError::FileNotFound) => None,
            Err(e) => return Err(e.into()),
        };
        Ok((xml, json))
    } else if raw.starts_with(&MAGIC.to_le_bytes()) {
        Ok((container(raw)?, None))
    } else {
        bail!(
            "Unexpected configuration format ({} bytes); check Miniserver full-access permissions",
            raw.len()
        )
    }
}
fn entry(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Result<Vec<u8>> {
    bounded_read(
        zip.by_name(name)
            .with_context(|| format!("Missing {name} in deployment ZIP"))?,
    )
}
pub fn bounded_read(reader: impl Read) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    reader
        .take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut out)?;
    ensure!(
        out.len() <= MAX_CONFIG_BYTES,
        "Decompressed configuration exceeds size limit"
    );
    Ok(out)
}
pub fn container(raw: &[u8]) -> Result<Vec<u8>> {
    ensure!(raw.len() >= 16, "Truncated LoxCC header");
    let word = |at| u32::from_le_bytes(raw[at..at + 4].try_into().unwrap());
    ensure!(word(0) == MAGIC, "Invalid LoxCC magic");
    let compressed = word(4) as usize;
    let size = word(8) as usize;
    let crc = word(12);
    ensure!(
        size <= MAX_CONFIG_BYTES && compressed <= MAX_CONFIG_BYTES,
        "LoxCC declared size exceeds limit"
    );
    let payload = raw
        .get(16..16 + compressed)
        .context("Truncated LoxCC payload")?;
    let magic = payload
        .get(..4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()));
    let framed = magic.is_some_and(|m| {
        m == 0x184d2204 || m == 0x184c2102 || (0x184d2a50..=0x184d2a5f).contains(&m)
    });
    let out = if framed {
        bounded_read(lz4_flex::frame::FrameDecoder::new(payload))?
    } else {
        let mut out = vec![0u8; size];
        match lz4_flex::block::decompress_into(payload, &mut out) {
            Ok(n) => {
                out.truncate(n);
                out
            }
            Err(block) => bounded_read(lz4_flex::frame::FrameDecoder::new(payload))
                .with_context(|| format!("LZ4 block decoding: {block}"))?,
        }
    };
    ensure!(out.len() == size, "LoxCC uncompressed length mismatch");
    ensure!(crc32fast::hash(&out) == crc, "LoxCC checksum mismatch");
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub fn wrap(xml: &[u8]) -> Vec<u8> {
        let body = lz4_flex::block::compress(xml);
        let mut out = Vec::new();
        for n in [
            MAGIC,
            body.len() as u32,
            xml.len() as u32,
            crc32fast::hash(xml),
        ] {
            out.extend(n.to_le_bytes());
        }
        out.extend(body);
        out
    }
    #[test]
    fn block_and_zip_roundtrip() {
        use std::io::Write;
        let raw = wrap(b"<Root/>");
        assert_eq!(decode(&raw).unwrap().0, b"<Root/>");
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("sps0.LoxCC", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(&raw).unwrap();
        zip.start_file("LoxAPP3.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(br#"{"lastModified":"2026-10-30 12:00:00"}"#)
            .unwrap();
        let (xml, json) = decode(&zip.finish().unwrap().into_inner()).unwrap();
        assert_eq!(xml, b"<Root/>");
        assert!(json.unwrap().get("lastModified").is_some());
    }
    #[test]
    fn corrupt_truncated_and_oversized_containers_fail() {
        let good = wrap(b"<Root/>");
        for n in 0..good.len() {
            assert!(decode(&good[..n]).is_err());
        }
        let mut bad = good.clone();
        bad[12] ^= 1;
        assert!(decode(&bad).is_err());
        bad = good.clone();
        bad[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&bad).is_err());
        bad = good;
        bad[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert!(decode(&bad).is_err());
    }
    #[test]
    fn frame_roundtrip() {
        use std::io::Write;
        let xml = b"<Root/>";
        let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
        encoder.write_all(xml).unwrap();
        let body = encoder.finish().unwrap();
        let mut raw = wrap(xml);
        raw.truncate(16);
        raw[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
        raw.extend(body);
        assert_eq!(decode(&raw).unwrap().0, xml);
    }
    #[test]
    fn archive_and_container_truncations_and_header_corruption_never_panic() {
        use std::io::Write;
        let bare = wrap(b"<Root/>");
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("sps0.LoxCC", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(&bare).unwrap();
        let zipped = zip.finish().unwrap().into_inner();
        for full in [bare, zipped] {
            for cut in 0..full.len() {
                let _ = decode(&full[..cut]);
            }
            for at in 0..full.len().min(64) {
                let mut damaged = full.clone();
                damaged[at] ^= 0xff;
                let _ = decode(&damaged);
            }
        }
    }
}
