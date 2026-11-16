use sha2::Digest;

use crate::AppResult;

pub(crate) struct TorrentMetainfo<'a> {
    root: &'a [u8],
    info: &'a [u8],
    version_two: bool,
}

impl<'a> TorrentMetainfo<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> AppResult<Self> {
        let invalid = "attachment is not a valid torrent file";
        let mut cursor = 0;
        if bytes.first() != Some(&b'd')
            || skip_value(bytes, &mut cursor, 0).is_none()
            || cursor != bytes.len()
        {
            return Err(invalid.into());
        }
        let mut entries = dictionary(bytes).filter(|(key, _)| *key == b"info");
        let info = entries
            .next()
            .map(|(_, info)| info)
            .filter(|info| info.first() == Some(&b'd'))
            .ok_or(invalid)?;
        if entries.next().is_some() {
            return Err(invalid.into());
        }
        let version = field(info, b"meta version");
        if version.is_some_and(|version| version != b"i2e") {
            return Err("unsupported torrent metadata version".into());
        }
        Ok(Self {
            root: bytes,
            info,
            version_two: version.is_some(),
        })
    }

    pub(crate) fn magnet(&self) -> AppResult<String> {
        let mut magnet = if self.version_two {
            format!(
                "magnet:?xt=urn:btmh:1220{:x}",
                sha2::Sha256::digest(self.info)
            )
        } else {
            format!("magnet:?xt=urn:btih:{:x}", sha1::Sha1::digest(self.info))
        };
        if self.version_two && field(self.info, b"pieces").is_some() {
            magnet.push_str(&format!("&xt=urn:btih:{:x}", sha1::Sha1::digest(self.info)));
        }

        let mut trackers = Vec::new();
        for (key, value) in dictionary(self.root) {
            if key != b"announce" && key != b"announce-list" {
                continue;
            }
            let mut cursor = 0;
            while cursor < value.len() && trackers.len() < 32 {
                if !value[cursor].is_ascii_digit() {
                    cursor += 1;
                    continue;
                }
                let Some(raw) = take_string(value, &mut cursor) else {
                    break;
                };
                if let Ok(text) = std::str::from_utf8(raw) {
                    if reqwest::Url::parse(text).is_ok_and(|url| {
                        matches!(url.scheme(), "http" | "https" | "udp" | "wss")
                            && url.host_str().is_some()
                    }) && !trackers.contains(&text)
                    {
                        trackers.push(text);
                    }
                }
            }
        }
        for tracker in trackers {
            let parameter =
                reqwest::Url::parse_with_params("https://example.invalid/", [("tr", tracker)])?;
            let query = parameter.query().unwrap();
            if magnet.len() + query.len() + 1 <= 16 * 1024 {
                magnet.push('&');
                magnet.push_str(query);
            }
        }
        Ok(magnet)
    }

    pub(crate) fn file_names(&self) -> Vec<String> {
        let encoding = field(self.root, b"encoding")
            .and_then(string)
            .and_then(encoding_rs::Encoding::for_label);
        let mut names = Vec::new();
        if let Some(files) = field(self.info, b"files") {
            for file in list(files).filter(|file| !padding(file)) {
                let name = [b"path.utf-8".as_slice(), b"path"]
                    .into_iter()
                    .filter_map(|key| field(file, key))
                    .find_map(|path| list(path).last().and_then(string));
                names.extend(name);
            }
        }
        if let Some(tree) = field(self.info, b"file tree") {
            tree_names(tree, b"", &mut names);
        }
        if field(self.info, b"files").is_none() && !self.version_two {
            names.extend(
                field(self.info, b"name.utf-8")
                    .and_then(string)
                    .or_else(|| field(self.info, b"name").and_then(string)),
            );
        }

        let mut names: Vec<String> = names
            .into_iter()
            .filter(|name| !name.is_empty() && *name != b"." && *name != b"..")
            .map(|name| {
                let decoded = match std::str::from_utf8(name) {
                    Ok(text) => std::borrow::Cow::Borrowed(text),
                    Err(_) => encoding.map_or_else(
                        || String::from_utf8_lossy(name),
                        |encoding| encoding.decode_without_bom_handling(name).0,
                    ),
                };
                decoded.replace(char::is_control, " ").trim().to_owned()
            })
            .filter(|name| !name.is_empty())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

fn padding(file: &[u8]) -> bool {
    field(file, b"attr")
        .and_then(string)
        .is_some_and(|attr| attr.contains(&b'p'))
}

fn tree_names<'a>(tree: &'a [u8], name: &'a [u8], names: &mut Vec<&'a [u8]>) {
    for (child, entry) in dictionary(tree) {
        if child.is_empty() {
            if field(entry, b"length").is_some() && !padding(entry) {
                names.push(name);
            }
        } else {
            tree_names(entry, child, names);
        }
    }
}

fn field<'a>(dict: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    dictionary(dict).find_map(|(name, value)| (name == key).then_some(value))
}

fn dictionary(bytes: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut cursor = (bytes.first() == Some(&b'd')).then_some(1);
    std::iter::from_fn(move || {
        let index = cursor.as_mut()?;
        let key = take_string(bytes, index)?;
        Some((key, take_value(bytes, index)?))
    })
}

fn list(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut cursor = (bytes.first() == Some(&b'l')).then_some(1);
    std::iter::from_fn(move || take_value(bytes, cursor.as_mut()?))
}

fn take_value<'a>(bytes: &'a [u8], index: &mut usize) -> Option<&'a [u8]> {
    let start = *index;
    skip_value(bytes, index, 0)?;
    Some(&bytes[start..*index])
}

fn string(bytes: &[u8]) -> Option<&[u8]> {
    take_string(bytes, &mut 0)
}

fn take_string<'a>(bytes: &'a [u8], index: &mut usize) -> Option<&'a [u8]> {
    let start = *index;
    while bytes.get(*index)?.is_ascii_digit() {
        *index += 1;
    }
    if *index == start
        || bytes.get(*index) != Some(&b':')
        || (*index > start + 1 && bytes[start] == b'0')
    {
        return None;
    }
    let length = std::str::from_utf8(&bytes[start..*index])
        .ok()?
        .parse::<usize>()
        .ok()?;
    *index += 1;
    let end = index.checked_add(length)?;
    let result = bytes.get(*index..end)?;
    *index = end;
    Some(result)
}

fn skip_value(bytes: &[u8], index: &mut usize, depth: usize) -> Option<()> {
    if depth > 64 {
        return None;
    }
    match *bytes.get(*index)? {
        b'i' => {
            *index += 1;
            let start = *index;
            if bytes.get(*index) == Some(&b'-') {
                *index += 1;
            }
            let digits = *index;
            while bytes.get(*index)?.is_ascii_digit() {
                *index += 1;
            }
            if *index == digits
                || bytes.get(*index) != Some(&b'e')
                || (*index > digits + 1 && bytes[digits] == b'0')
                || (digits != start && bytes[digits] == b'0')
            {
                return None;
            }
            *index += 1;
        }
        b'l' | b'd' => {
            let dict = bytes[*index] == b'd';
            *index += 1;
            while bytes.get(*index) != Some(&b'e') {
                if dict {
                    take_string(bytes, index)?;
                }
                skip_value(bytes, index, depth + 1)?;
            }
            *index += 1;
        }
        b'0'..=b'9' => {
            take_string(bytes, index)?;
        }
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_unicode_names_for_single_file_and_multifile_torrents() {
        let single = "d4:infod6:lengthi1e4:name8:demo.mp410:name.utf-814:Фильм.mp4ee";
        assert_eq!(
            TorrentMetainfo::parse(single.as_bytes())
                .unwrap()
                .file_names(),
            ["Фильм.mp4"]
        );
        let file = "d6:lengthi1e4:pathl3:dir8:demo.mp4ee";
        let padding = "d4:attr1:p6:lengthi1e4:pathl4:.pad1:0ee";
        let torrent = format!("d4:infod5:filesl{file}{padding}e4:name7:displayee");
        assert_eq!(
            TorrentMetainfo::parse(torrent.as_bytes())
                .unwrap()
                .file_names(),
            ["demo.mp4"]
        );
    }

    #[test]
    fn indexes_nested_v2_and_hybrid_entries_without_duplicate_names() {
        let leaf = "d0:d6:lengthi1eee";
        let tree = format!("d3:dird8:demo.mp4{leaf}ee");
        for v1_files in ["", "5:filesld6:lengthi1e4:pathl8:demo.mp4eee"] {
            let torrent =
                format!("d4:infod9:file tree{tree}{v1_files}12:meta versioni2e4:name7:displayee");
            assert_eq!(
                TorrentMetainfo::parse(torrent.as_bytes())
                    .unwrap()
                    .file_names(),
                ["demo.mp4"]
            );
        }
    }
}
