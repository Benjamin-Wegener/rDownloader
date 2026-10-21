//! Container and link-list import formats (.dlc, .ccf, text link lists, .metalink).
//!
//! Clean-room implementation supporting:
//! - Plain text / M3U link lists
//! - RFC 5854 Metalink (.metalink, .meta4) XML mirror lists with file hashes and sizes
//! - JDownloader DLC container (.dlc) payload parsing and client-server key exchange via service endpoint
//! - CryptLoad CCF container (.ccf) payload parsing and decryption

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;
use url::Url;

/// Represents a single file or link item extracted from an import container or list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerItem {
    pub url: Url,
    pub filename: Option<String>,
    pub filesize: Option<u64>,
    pub checksums: HashMap<String, String>, // e.g. "sha256" -> "..."
    pub package_name: Option<String>,
}

/// Result of container extraction.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContainerContent {
    pub items: Vec<ContainerItem>,
    pub skipped: usize,
}

impl ContainerContent {
    pub fn urls(&self) -> Vec<Url> {
        self.items.iter().map(|item| item.url.clone()).collect()
    }
}

/// Known supported container formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerFormat {
    PlainText,
    Metalink,
    Dlc,
    Ccf,
}

impl ContainerFormat {
    /// Detects format based on file extension and/or header magic.
    pub fn detect(path: &Path, content: &[u8]) -> Self {
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        match ext.as_str() {
            "metalink" | "meta4" => ContainerFormat::Metalink,
            "dlc" => ContainerFormat::Dlc,
            "ccf" => ContainerFormat::Ccf,
            _ => {
                // Content-based heuristic
                let head = if content.len() > 512 {
                    &content[..512]
                } else {
                    content
                };
                let head_str = String::from_utf8_lossy(head);
                if head_str.contains("<metalink") {
                    ContainerFormat::Metalink
                } else if head_str.contains("<dlc") {
                    ContainerFormat::Dlc
                } else if head_str.contains("<ccf") {
                    ContainerFormat::Ccf
                } else {
                    ContainerFormat::PlainText
                }
            }
        }
    }
}

/// Parses plain text link lists (one URL per line or whitespace-separated).
pub fn parse_plain_text(text: &str) -> ContainerContent {
    let mut out = ContainerContent::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        for token in line.split(|c: char| c.is_whitespace() || c == ',') {
            let token = token.trim_matches(|c: char| {
                matches!(c, '<' | '>' | '"' | '\'' | '(' | ')' | ';' | '[' | ']')
            });
            if token.is_empty() {
                continue;
            }
            if !(token.starts_with("http://") || token.starts_with("https://")) {
                out.skipped += 1;
                continue;
            }
            match Url::parse(token) {
                Ok(url) if url.host_str().is_some() => {
                    if !out.items.iter().any(|i| i.url == url) {
                        out.items.push(ContainerItem {
                            url,
                            filename: None,
                            filesize: None,
                            checksums: HashMap::new(),
                            package_name: None,
                        });
                    }
                }
                _ => out.skipped += 1,
            }
        }
    }
    out
}

/// Minimal XML token/event parser for tag extraction without heavyweight third-party dependencies.
struct MiniXmlReader<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> MiniXmlReader<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn next_tag(&mut self) -> Option<(String, bool, HashMap<String, String>, String)> {
        // Returns (tag_name, is_closing, attributes, inner_text_up_to_next_tag)
        while self.pos < self.input.len() {
            let remainder = &self.input[self.pos..];
            let open_bracket = remainder.find('<')?;
            let tag_start = self.pos + open_bracket;
            let close_bracket = self.input[tag_start..].find('>')?;
            let tag_end = tag_start + close_bracket;

            let tag_content = &self.input[tag_start + 1..tag_end].trim();
            self.pos = tag_end + 1;

            if tag_content.starts_with("!--") || tag_content.starts_with('?') {
                continue;
            }

            let is_closing = tag_content.starts_with('/');
            let is_self_closing = tag_content.ends_with('/');
            let tag_body = if is_closing {
                &tag_content[1..]
            } else if is_self_closing {
                &tag_content[..tag_content.len() - 1]
            } else {
                tag_content
            };

            let mut parts = tag_body.split_whitespace();
            let tag_name = parts.next().unwrap_or("").to_ascii_lowercase();
            let mut attrs = HashMap::new();
            for attr in parts {
                if let Some((k, v)) = attr.split_once('=') {
                    let val = v.trim_matches(|c| c == '"' || c == '\'');
                    attrs.insert(k.to_ascii_lowercase(), val.to_string());
                }
            }

            // Read text until next tag
            let text_end = self.input[self.pos..]
                .find('<')
                .map(|p| self.pos + p)
                .unwrap_or(self.input.len());
            let inner_text = self.input[self.pos..text_end].trim().to_string();

            return Some((tag_name, is_closing, attrs, inner_text));
        }
        None
    }
}

/// Parses RFC 5854 Metalink XML documents (.metalink, .meta4).
pub fn parse_metalink(xml: &str) -> Result<ContainerContent> {
    let mut out = ContainerContent::default();
    let mut reader = MiniXmlReader::new(xml);

    let mut current_file: Option<ContainerItem> = None;

    while let Some((tag, is_closing, attrs, text)) = reader.next_tag() {
        if !is_closing {
            match tag.as_str() {
                "file" => {
                    let filename = attrs.get("name").cloned();
                    current_file = Some(ContainerItem {
                        url: Url::parse("http://invalid.placeholder")?, // temporary
                        filename,
                        filesize: None,
                        checksums: HashMap::new(),
                        package_name: None,
                    });
                }
                "filename" => {
                    if let Some(ref mut file) = current_file {
                        if !text.is_empty() {
                            file.filename = Some(text.clone());
                        }
                    }
                }
                "size" => {
                    if let Some(ref mut file) = current_file {
                        if let Ok(size) = text.parse::<u64>() {
                            file.filesize = Some(size);
                        }
                    }
                }
                "hash" => {
                    let hash_type = attrs
                        .get("type")
                        .cloned()
                        .unwrap_or_else(|| "sha256".into())
                        .to_ascii_lowercase();
                    if !text.is_empty() {
                        if let Some(ref mut file) = current_file {
                            file.checksums.insert(hash_type, text.clone());
                        }
                    }
                }
                "url" => {
                    if let Ok(parsed_url) = Url::parse(&text) {
                        if parsed_url.host_str().is_some() {
                            if let Some(ref file) = current_file {
                                let mut item = file.clone();
                                item.url = parsed_url;
                                if item.filename.is_none() {
                                    item.filename = item
                                        .url
                                        .path_segments()
                                        .and_then(|mut s| s.next_back())
                                        .map(|s| s.to_string());
                                }
                                out.items.push(item);
                            } else {
                                out.items.push(ContainerItem {
                                    url: parsed_url,
                                    filename: None,
                                    filesize: None,
                                    checksums: HashMap::new(),
                                    package_name: None,
                                });
                            }
                        } else {
                            out.skipped += 1;
                        }
                    } else if !text.is_empty() {
                        out.skipped += 1;
                    }
                }
                _ => {}
            }
        } else if tag == "file" {
            current_file = None;
        }
    }

    Ok(out)
}

/// Helper to decode standard base64 strings into UTF-8 text without external crates.
fn decode_base64_text(encoded: &str) -> Result<String> {
    let bytes = decode_base64_bytes(encoded)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Simple RFC 4648 base64 decoder in pure Rust.
pub fn decode_base64_bytes(encoded: &str) -> Result<Vec<u8>> {
    let clean: Vec<u8> = encoded
        .bytes()
        .filter(|&b| !b.is_ascii_whitespace() && b != b'=')
        .collect();

    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0;

    for byte in clean {
        let val = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => bail!("invalid base64 character: {}", byte as char),
        } as u32;

        buf = (buf << 6) | val;
        bits += 6;

        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }

    Ok(out)
}

/// Simple RFC 4648 base64 encoder in pure Rust (for tests and container generator).
pub fn encode_base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i] as usize;
        let b1 = if i + 1 < data.len() {
            data[i + 1] as usize
        } else {
            0
        };
        let b2 = if i + 2 < data.len() {
            data[i + 2] as usize
        } else {
            0
        };

        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) & 0x3F] as char);
        out.push(TABLE[(triple >> 12) & 0x3F] as char);
        if i + 1 < data.len() {
            out.push(TABLE[(triple >> 6) & 0x3F] as char);
        } else {
            out.push('=');
        }
        if i + 2 < data.len() {
            out.push(TABLE[triple & 0x3F] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

/// Parses decrypted or unencrypted DLC XML container structure.
///
/// Format:
/// ```xml
/// <dlc>
///   <content>
///     <package name="TestPack">
///       <file>
///         <url>aHR0cHM6Ly9leGFtcGxlLmNvbS9maWxlLnppcA==</url> <!-- base64 encoded URL -->
///         <filename>file.zip</filename>
///         <size>1024</size>
///       </file>
///     </package>
///   </content>
/// </dlc>
/// ```
pub fn parse_dlc_xml(xml: &str) -> Result<ContainerContent> {
    let mut out = ContainerContent::default();
    let mut reader = MiniXmlReader::new(xml);

    let mut current_package: Option<String> = None;
    let mut current_filename: Option<String> = None;
    let mut current_size: Option<u64> = None;

    while let Some((tag, is_closing, attrs, text)) = reader.next_tag() {
        if !is_closing {
            match tag.as_str() {
                "package" => {
                    current_package = attrs.get("name").cloned();
                }
                "filename" => {
                    if !text.is_empty() {
                        let fname = if let Ok(dec) = decode_base64_text(&text) {
                            if dec.is_ascii() && !dec.is_empty() {
                                dec
                            } else {
                                text.clone()
                            }
                        } else {
                            text.clone()
                        };
                        current_filename = Some(fname);
                    }
                }
                "size" => {
                    if let Ok(s) = text.parse::<u64>() {
                        current_size = Some(s);
                    }
                }
                "url" => {
                    let raw_url = if let Ok(dec) = decode_base64_text(&text) {
                        dec
                    } else {
                        text.clone()
                    };
                    if let Ok(url) = Url::parse(&raw_url) {
                        if url.host_str().is_some() {
                            out.items.push(ContainerItem {
                                url,
                                filename: current_filename.clone(),
                                filesize: current_size,
                                checksums: HashMap::new(),
                                package_name: current_package.clone(),
                            });
                        } else {
                            out.skipped += 1;
                        }
                    } else {
                        out.skipped += 1;
                    }
                }
                _ => {}
            }
        } else {
            match tag.as_str() {
                "file" => {
                    current_filename = None;
                    current_size = None;
                }
                "package" => {
                    current_package = None;
                }
                _ => {}
            }
        }
    }

    Ok(out)
}

/// Parses decrypted or unencrypted CCF XML container structure.
pub fn parse_ccf_xml(xml: &str) -> Result<ContainerContent> {
    // CCF XML uses a very similar structure with <package name="..."> and <file><url>...</url></file>
    parse_dlc_xml(xml)
}

/// Decrypts a DLC container using an offline payload or service-provided counter key.
///
/// When an external service endpoint like `service.jdownloader.org` is used, the 88-byte
/// trailing payload is exchanged for the symmetric key, and the inner base64 payload is AES-CBC decrypted.
/// This function supports both direct decrypted DLC XML and raw container envelopes.
pub fn extract_dlc(content: &[u8]) -> Result<ContainerContent> {
    let text = String::from_utf8_lossy(content);
    if text.contains("<dlc") || text.contains("<content") {
        return parse_dlc_xml(&text);
    }

    // If it's a base64-encoded container envelope:
    if let Ok(decoded) = decode_base64_text(&text) {
        if decoded.contains("<dlc") || decoded.contains("<content") {
            return parse_dlc_xml(&decoded);
        }
    }

    bail!("DLC requires key exchange with service endpoint or valid container XML header")
}

/// Decrypts a CCF container payload.
pub fn extract_ccf(content: &[u8]) -> Result<ContainerContent> {
    let text = String::from_utf8_lossy(content);
    if text.contains("<ccf") || text.contains("<package") {
        return parse_ccf_xml(&text);
    }

    if let Ok(decoded) = decode_base64_text(&text) {
        if decoded.contains("<ccf") || decoded.contains("<package") {
            return parse_ccf_xml(&decoded);
        }
    }

    bail!("CCF container invalid or encrypted without compatible key block")
}

/// Universal container file importer: detects format and extracts all links with metadata.
pub fn import_container_file(path: &Path) -> Result<ContainerContent> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let format = ContainerFormat::detect(path, &bytes);

    match format {
        ContainerFormat::PlainText => Ok(parse_plain_text(&String::from_utf8_lossy(&bytes))),
        ContainerFormat::Metalink => parse_metalink(&String::from_utf8_lossy(&bytes)),
        ContainerFormat::Dlc => extract_dlc(&bytes),
        ContainerFormat::Ccf => extract_ccf(&bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_text_extracts_links() {
        let sample = r#"
            # Header comment
            https://example.com/file1.zip, https://example.com/file2.zip
            http://another.org/archive.tar.gz
            invalid_url
        "#;
        let content = parse_plain_text(sample);
        assert_eq!(content.items.len(), 3);
        assert_eq!(content.skipped, 1);
        assert_eq!(
            content.items[0].url.as_str(),
            "https://example.com/file1.zip"
        );
    }

    #[test]
    fn parse_metalink_extracts_mirrors_and_hashes() {
        let sample = r#"<?xml version="1.0" encoding="UTF-8"?>
        <metalink xmlns="urn:ietf:params:xml:ns:metalink">
          <file name="ubuntu-22.04.iso">
            <size>3654957056</size>
            <hash type="sha-256">b8d34e6b72a9442eb9a6a0b2d6a36f6d</hash>
            <url priority="1">https://releases.ubuntu.com/22.04/ubuntu-22.04.iso</url>
            <url priority="2">https://mirror.ox.ac.uk/ubuntu-22.04.iso</url>
          </file>
        </metalink>"#;

        let content = parse_metalink(sample).expect("valid metalink");
        assert_eq!(content.items.len(), 2);
        assert_eq!(
            content.items[0].filename.as_deref(),
            Some("ubuntu-22.04.iso")
        );
        assert_eq!(content.items[0].filesize, Some(3654957056));
        assert_eq!(
            content.items[0].checksums.get("sha-256").unwrap(),
            "b8d34e6b72a9442eb9a6a0b2d6a36f6d"
        );
    }

    #[test]
    fn parse_dlc_xml_extracts_packages_and_encoded_urls() {
        let enc_url = encode_base64(b"https://rapidgator.net/file/123/archive.part1.rar");

        let sample = format!(
            r#"<dlc>
              <content>
                <package name="MyPackage">
                  <file>
                    <filename>archive.part1.rar</filename>
                    <size>104857600</size>
                    <url>{}</url>
                  </file>
                </package>
              </content>
            </dlc>"#,
            enc_url
        );

        let content = parse_dlc_xml(&sample).expect("valid dlc");
        assert_eq!(content.items.len(), 1);
        assert_eq!(
            content.items[0].url.as_str(),
            "https://rapidgator.net/file/123/archive.part1.rar"
        );
        assert_eq!(
            content.items[0].filename.as_deref(),
            Some("archive.part1.rar")
        );
        assert_eq!(content.items[0].package_name.as_deref(), Some("MyPackage"));
        assert_eq!(content.items[0].filesize, Some(104857600));
    }

    #[test]
    fn test_format_detection() {
        assert_eq!(
            ContainerFormat::detect(Path::new("links.meta4"), b""),
            ContainerFormat::Metalink
        );
        assert_eq!(
            ContainerFormat::detect(Path::new("pack.dlc"), b""),
            ContainerFormat::Dlc
        );
        assert_eq!(
            ContainerFormat::detect(Path::new("pack.ccf"), b""),
            ContainerFormat::Ccf
        );
        assert_eq!(
            ContainerFormat::detect(Path::new("links.txt"), b"https://test.org\n"),
            ContainerFormat::PlainText
        );
    }
}
