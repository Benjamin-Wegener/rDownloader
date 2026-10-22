//! HLS playlist parsing (`application/vnd.apple.mpegurl`).
//!
//! Supports master playlists (variant streams) and media playlists, including
//! `#EXT-X-MAP` init segments and `#EXT-X-BYTERANGE` sub-ranges. Encrypted
//! streams (`AES-128`, `SAMPLE-AES`) are rejected: we have no key handling.
//!
//! ```ignore
//! let playlist = hls::parse(&url, &body)?.expect("looks like m3u8");
//! ```

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use url::Url;

use super::segment::SegmentRef;
use crate::contract::ByteRange;
use crate::t;

/// One variant stream of a master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsVariant {
    pub url: Url,
    pub bandwidth: Option<u64>,
    pub resolution: Option<(u32, u32)>,
}

/// A media playlist: optional init segment plus the media segments in order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HlsMedia {
    pub init: Option<SegmentRef>,
    pub segments: Vec<SegmentRef>,
    pub duration_secs: Option<f64>,
    /// Playlist without `#EXT-X-ENDLIST` (still a snapshot of what it lists).
    pub live: bool,
}

/// Parsed playlist: either a master or a media playlist.
#[derive(Debug, Clone, PartialEq)]
pub enum HlsPlaylist {
    Master(Vec<HlsVariant>),
    Media(HlsMedia),
}

impl HlsPlaylist {
    /// Picks the variant with the highest bandwidth (resolution as tie-break).
    pub fn best_variant(variants: &[HlsVariant]) -> Option<&HlsVariant> {
        variants.iter().max_by_key(|variant| {
            let pixels = variant
                .resolution
                .map_or(0u64, |(w, h)| u64::from(w) * u64::from(h));
            (variant.bandwidth.unwrap_or(0), pixels)
        })
    }
}

/// Parses an HLS playlist. `Ok(None)` means the text is not an HLS playlist.
pub fn parse(base: &Url, text: &str) -> Result<Option<HlsPlaylist>> {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let Some(first) = lines.next() else {
        return Ok(None);
    };
    if !first.starts_with("#EXTM3U") {
        return Ok(None);
    }

    let mut master = false;
    let mut variants = Vec::new();
    let mut pending_variant: Option<HashMap<String, String>> = None;
    let mut media = HlsMedia::default();
    let mut pending_duration: Option<f64> = None;
    let mut pending_range: Option<(u64, Option<u64>)> = None;
    let mut last_end: HashMap<String, u64> = HashMap::new();
    let mut endlist = false;

    for line in lines {
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            master = true;
            pending_variant = Some(attributes(attrs));
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let attrs = attributes(rest);
            match attrs.get("METHOD").map(String::as_str) {
                None | Some("NONE") => {}
                Some(_) => bail!(t!("net.segment.encrypted")),
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = attributes(rest);
            let uri = attrs.get("URI").ok_or_else(|| {
                anyhow::anyhow!(t!("net.segment.invalid", detail = "EXT-X-MAP without URI"))
            })?;
            let url = join(base, uri)?;
            let byte_range = match attrs.get("BYTERANGE") {
                Some(value) => {
                    let (length, offset) = parse_range_attr(value)?;
                    Some(checked_range(length, offset.unwrap_or(0))?)
                }
                None => None,
            };
            media.init = Some(SegmentRef { url, byte_range });
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range = Some(parse_range_attr(rest.trim())?);
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending_duration = rest
                .split(',')
                .next()
                .and_then(|value| value.trim().parse::<f64>().ok());
            continue;
        }
        if line.starts_with("#EXT-X-ENDLIST") {
            endlist = true;
            continue;
        }
        if line.starts_with('#') {
            continue;
        }

        if let Some(attrs) = pending_variant.take() {
            variants.push(HlsVariant {
                url: join(base, line)?,
                bandwidth: attrs.get("BANDWIDTH").and_then(|v| v.trim().parse().ok()),
                resolution: attrs.get("RESOLUTION").and_then(|v| parse_resolution(v)),
            });
            continue;
        }
        if master {
            continue;
        }

        let url = join(base, line)?;
        let byte_range = match pending_range.take() {
            Some((length, offset)) => {
                let key = url.as_str().to_owned();
                let start = offset.or_else(|| last_end.get(&key).copied()).unwrap_or(0);
                let byte_range = checked_range(length, start)?;
                last_end.insert(key, byte_range.end.unwrap() + 1);
                Some(byte_range)
            }
            None => None,
        };
        media.segments.push(SegmentRef { url, byte_range });
        if let Some(duration) = pending_duration.take() {
            *media.duration_secs.get_or_insert(0.0) += duration;
        }
    }

    if master {
        if variants.is_empty() {
            bail!(t!("net.segment.empty"));
        }
        return Ok(Some(HlsPlaylist::Master(variants)));
    }
    if media.segments.is_empty() {
        bail!(t!("net.segment.empty"));
    }
    media.live = !endlist;
    Ok(Some(HlsPlaylist::Media(media)))
}

fn join(base: &Url, uri: &str) -> Result<Url> {
    let uri = uri.trim();
    if uri.is_empty() {
        bail!(t!("net.segment.invalid", detail = "empty URI"));
    }
    base.join(uri)
        .with_context(|| t!("net.segment.invalid", detail = format!("bad URI {uri:?}")))
}

fn checked_range(length: u64, start: u64) -> Result<ByteRange> {
    if length == 0 {
        bail!(t!("net.segment.invalid", detail = "empty EXT-X-BYTERANGE"));
    }
    let end = start
        .checked_add(length)
        .and_then(|end| end.checked_sub(1))
        .ok_or_else(|| anyhow::anyhow!(t!("net.segment.invalid", detail = "BYTERANGE overflow")))?;
    Ok(ByteRange {
        start,
        end: Some(end),
    })
}

fn parse_resolution(value: &str) -> Option<(u32, u32)> {
    let (width, height) = value.trim().split_once(['x', 'X'])?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

/// Parses `#EXT-X-BYTERANGE`-style values: `length[@offset]`.
fn parse_range_attr(value: &str) -> Result<(u64, Option<u64>)> {
    let value = value.trim().trim_matches('"');
    let (length, offset) = match value.split_once('@') {
        Some((length, offset)) => (length, Some(offset)),
        None => (value, None),
    };
    let length: u64 = length.trim().parse().map_err(|_| {
        anyhow::anyhow!(t!(
            "net.segment.invalid",
            detail = format!("bad BYTERANGE {value:?}")
        ))
    })?;
    if length == 0 {
        bail!(t!("net.segment.invalid", detail = "zero-length BYTERANGE"));
    }
    let offset = match offset {
        Some(offset) => Some(offset.trim().parse().map_err(|_| {
            anyhow::anyhow!(t!(
                "net.segment.invalid",
                detail = format!("bad BYTERANGE {value:?}")
            ))
        })?),
        None => None,
    };
    Ok((length, offset))
}

/// Parses `KEY=VALUE` attribute lists; quoted values may contain commas.
fn attributes(input: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ',') {
            i += 1;
        }
        let key_start = i;
        while i < chars.len() && chars[i] != '=' && chars[i] != ',' {
            i += 1;
        }
        if i >= chars.len() || chars[i] != '=' {
            break;
        }
        let key = chars[key_start..i].iter().collect::<String>();
        i += 1;
        let value = if i < chars.len() && chars[i] == '"' {
            i += 1;
            let value_start = i;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            let value = chars[value_start..i].iter().collect::<String>();
            if i < chars.len() {
                i += 1;
            }
            value
        } else {
            let value_start = i;
            while i < chars.len() && chars[i] != ',' {
                i += 1;
            }
            chars[value_start..i].iter().collect::<String>()
        };
        let key = key.trim();
        if !key.is_empty() {
            map.insert(key.to_ascii_uppercase(), value.trim().to_owned());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.test/path/index.m3u8").unwrap()
    }

    #[test]
    fn ignores_foreign_documents() {
        assert!(parse(&base(), "<html></html>").unwrap().is_none());
        assert!(parse(&base(), "").unwrap().is_none());
    }

    #[test]
    fn parses_media_playlist_with_ranges() {
        let text = "\
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:10
#EXT-X-MAP:URI=\"init.mp4\"
#EXTINF:9.9,
seg0.ts
#EXTINF:8.5,
#EXT-X-BYTERANGE:100@10
seg1.ts
#EXT-X-ENDLIST
";
        let Some(HlsPlaylist::Media(media)) = parse(&base(), text).unwrap() else {
            panic!("expected media playlist");
        };
        assert!(!media.live);
        assert_eq!(media.duration_secs, Some(18.4));
        assert_eq!(
            media.init.as_ref().unwrap().url.as_str(),
            "https://cdn.test/path/init.mp4"
        );
        assert_eq!(media.segments.len(), 2);
        assert_eq!(
            media.segments[0].url.as_str(),
            "https://cdn.test/path/seg0.ts"
        );
        assert_eq!(media.segments[0].byte_range, None);
        assert_eq!(
            media.segments[1].byte_range,
            Some(ByteRange {
                start: 10,
                end: Some(109)
            })
        );
    }

    #[test]
    fn byte_range_offset_defaults_to_previous_end() {
        let text = "\
#EXTM3U
#EXT-X-BYTERANGE:100
seg.ts
#EXT-X-BYTERANGE:50
seg.ts
#EXT-X-ENDLIST
";
        let Some(HlsPlaylist::Media(media)) = parse(&base(), text).unwrap() else {
            panic!("expected media playlist");
        };
        assert_eq!(
            media.segments[0].byte_range,
            Some(ByteRange {
                start: 0,
                end: Some(99)
            })
        );
        assert_eq!(
            media.segments[1].byte_range,
            Some(ByteRange {
                start: 100,
                end: Some(149)
            })
        );
    }

    #[test]
    fn parses_master_playlist_and_picks_highest_bandwidth() {
        let text = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360
low.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720
high.m3u8
";
        let Some(HlsPlaylist::Master(variants)) = parse(&base(), text).unwrap() else {
            panic!("expected master playlist");
        };
        assert_eq!(variants.len(), 2);
        let best = HlsPlaylist::best_variant(&variants).unwrap();
        assert_eq!(best.url.as_str(), "https://cdn.test/path/high.m3u8");
        assert_eq!(best.resolution, Some((1280, 720)));
    }

    #[test]
    fn absolute_uris_and_quoted_values_with_commas() {
        let text = "\
#EXTM3U
#EXT-X-MAP:URI=\"https://other.test/i.mp4\",BYTERANGE=\"10@0\"
#EXTINF:4,
https://other.test/0.ts
#EXT-X-ENDLIST
";
        let Some(HlsPlaylist::Media(media)) = parse(&base(), text).unwrap() else {
            panic!("expected media playlist");
        };
        assert_eq!(
            media.init.as_ref().unwrap().url.as_str(),
            "https://other.test/i.mp4"
        );
        assert_eq!(
            media.init.as_ref().unwrap().byte_range,
            Some(ByteRange {
                start: 0,
                end: Some(9)
            })
        );
        assert_eq!(media.segments[0].url.as_str(), "https://other.test/0.ts");
    }

    #[test]
    fn encrypted_streams_are_rejected() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\nseg.ts\n#EXT-X-ENDLIST\n";
        assert!(parse(&base(), text).is_err());
    }

    #[test]
    fn live_playlist_is_flagged() {
        let text = "#EXTM3U\n#EXTINF:4,\nseg.ts\n";
        let Some(HlsPlaylist::Media(media)) = parse(&base(), text).unwrap() else {
            panic!("expected media playlist");
        };
        assert!(media.live);
    }

    #[test]
    fn empty_media_playlist_is_an_error() {
        assert!(parse(&base(), "#EXTM3U\n#EXT-X-ENDLIST\n").is_err());
    }
}
