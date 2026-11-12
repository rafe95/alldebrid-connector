//! Info-hash and name extraction for magnets and `.torrent` files.

use anyhow::{anyhow, bail, Result};
use sha1::{Digest, Sha1};

/// What we can learn about a torrent before sending it to AllDebrid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentMeta {
    /// Lowercase hex v1 info-hash (40 chars).
    pub hash: String,
    /// Display name, if known.
    pub name: Option<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn base32_to_hex(s: &str) -> Option<String> {
    let mut bits = 0u32;
    let mut acc = 0u64;
    let mut out = Vec::with_capacity(20);
    for c in s.bytes() {
        let v = match c.to_ascii_uppercase() {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        acc = (acc << 5) | u64::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(hex(&out))
}

/// Normalize a v1 info-hash given as 40 hex chars or 32 base32 chars.
pub fn normalize_hash(s: &str) -> Option<String> {
    match s.len() {
        40 if s.bytes().all(|b| b.is_ascii_hexdigit()) => Some(s.to_ascii_lowercase()),
        32 => base32_to_hex(s),
        _ => None,
    }
}

/// Parse a magnet URI (or a bare info-hash).
pub fn parse_magnet(uri: &str) -> Result<TorrentMeta> {
    let uri = uri.trim();
    if let Some(hash) = normalize_hash(uri) {
        return Ok(TorrentMeta { hash, name: None });
    }
    let query = uri
        .strip_prefix("magnet:?")
        .ok_or_else(|| anyhow!("not a magnet URI"))?;
    let mut hash = None;
    let mut name = None;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        match &*k {
            "xt" => {
                if let Some(h) = v.strip_prefix("urn:btih:").and_then(normalize_hash) {
                    hash = Some(h);
                }
            }
            "dn" if !v.is_empty() => name = Some(v.into_owned()),
            _ => {}
        }
    }
    let hash = hash.ok_or_else(|| anyhow!("magnet has no v1 info-hash (xt=urn:btih:...)"))?;
    Ok(TorrentMeta { hash, name })
}

// --- minimal bencode walking -------------------------------------------------

fn parse_str(b: &[u8], i: usize) -> Option<(&[u8], usize)> {
    let colon = i + b.get(i..)?.iter().position(|&c| c == b':')?;
    let len: usize = std::str::from_utf8(&b[i..colon]).ok()?.parse().ok()?;
    let end = colon.checked_add(1)?.checked_add(len)?;
    Some((b.get(colon + 1..end)?, end))
}

/// Index just past the bencoded value starting at `i`.
fn skip(b: &[u8], i: usize, depth: u32) -> Option<usize> {
    if depth > 64 {
        return None;
    }
    match *b.get(i)? {
        b'i' => Some(i + b[i..].iter().position(|&c| c == b'e')? + 1),
        b'l' => {
            let mut j = i + 1;
            while *b.get(j)? != b'e' {
                j = skip(b, j, depth + 1)?;
            }
            Some(j + 1)
        }
        b'd' => {
            let mut j = i + 1;
            while *b.get(j)? != b'e' {
                j = parse_str(b, j)?.1;
                j = skip(b, j, depth + 1)?;
            }
            Some(j + 1)
        }
        b'0'..=b'9' => Some(parse_str(b, i)?.1),
        _ => None,
    }
}

/// Byte range of the value for `key` in the dictionary starting at `start`.
fn dict_get(b: &[u8], start: usize, key: &[u8]) -> Option<(usize, usize)> {
    if *b.get(start)? != b'd' {
        return None;
    }
    let mut j = start + 1;
    while *b.get(j)? != b'e' {
        let (k, after_key) = parse_str(b, j)?;
        let end = skip(b, after_key, 0)?;
        if k == key {
            return Some((after_key, end));
        }
        j = end;
    }
    None
}

/// Compute the info-hash and name of a `.torrent` file.
pub fn parse_torrent(bytes: &[u8]) -> Result<TorrentMeta> {
    let Some((start, end)) = dict_get(bytes, 0, b"info") else {
        bail!("not a valid .torrent file");
    };
    let hash = hex(&Sha1::digest(&bytes[start..end]));
    let name = dict_get(bytes, start, b"name")
        .and_then(|(s, _)| parse_str(bytes, s))
        .map(|(n, _)| String::from_utf8_lossy(n).into_owned())
        .filter(|n| !n.is_empty());
    Ok(TorrentMeta { hash, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnet_hex_and_name() {
        let m = parse_magnet(
            "magnet:?xt=urn:btih:ABCDEF0123456789ABCDEF0123456789ABCDEF01&dn=My%20Show+S01&tr=x",
        )
        .unwrap();
        assert_eq!(m.hash, "abcdef0123456789abcdef0123456789abcdef01");
        assert_eq!(m.name.as_deref(), Some("My Show S01"));
    }

    #[test]
    fn magnet_base32() {
        // 20 zero bytes = 32 'A's in base32.
        let m = parse_magnet(&format!("magnet:?xt=urn:btih:{}", "A".repeat(32))).unwrap();
        assert_eq!(m.hash, "0".repeat(40));
    }

    #[test]
    fn bare_hash_and_garbage() {
        assert!(parse_magnet(&"a".repeat(40)).is_ok());
        assert!(parse_magnet("http://example.com").is_err());
        assert!(parse_magnet("magnet:?dn=nohash").is_err());
    }

    #[test]
    fn torrent_info_hash() {
        let info = b"d6:lengthi5e4:name8:file.iso12:piece lengthi16384e6:pieces0:e";
        let mut t = b"d8:announce3:foo4:info".to_vec();
        t.extend_from_slice(info);
        t.extend_from_slice(b"e");
        let m = parse_torrent(&t).unwrap();
        assert_eq!(m.hash, hex(&Sha1::digest(info)));
        assert_eq!(m.name.as_deref(), Some("file.iso"));
        assert!(parse_torrent(b"hello").is_err());
        assert!(parse_torrent(b"d4:infod").is_err());
    }
}
