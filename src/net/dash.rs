//! DASH manifest parsing (MPEG-DASH MPD, XML).
//!
//! Covers the common static VOD layouts: `SegmentTemplate` (with `$Number$` or
//! `SegmentTimeline`/`$Time$`), `SegmentList` and single-file `SegmentBase`.
//! Multi-period manifests are read up to the first period, DRM
//! (`ContentProtection`) and live (`type="dynamic"`) manifests are rejected.
//!
//! DASH usually separates audio and video; this module returns one plan per
//! representation and the caller picks one. Muxing is not part of the step.

use anyhow::{bail, Context, Result};
use roxmltree::{Document, Node};
use url::Url;

use super::segment::{SegmentKind, SegmentPlan, SegmentRef};
use crate::contract::ByteRange;
use crate::t;

/// Upper bound on generated segments, so a malicious manifest cannot make us
/// schedule millions of requests.
const MAX_SEGMENTS: usize = 100_000;

/// One DASH representation (a selectable quality).
#[derive(Debug, Clone, PartialEq)]
pub struct DashVariant {
    pub id: String,
    pub bandwidth: u64,
    pub mime_type: Option<String>,
    pub plan: SegmentPlan,
}

impl DashVariant {
    /// Highest bandwidth wins; this is the usual "best quality" default.
    pub fn best(variants: &[DashVariant]) -> Option<&DashVariant> {
        variants.iter().max_by_key(|variant| variant.bandwidth)
    }
}

/// Parses an MPD. `Ok(None)` means the document is not a DASH manifest.
pub fn parse(base: &Url, xml: &str) -> Result<Option<Vec<DashVariant>>> {
    let doc = Document::parse(xml)
        .with_context(|| t!("net.segment.invalid", detail = "malformed MPD XML"))?;
    let mpd = doc.root_element();
    if mpd.tag_name().name() != "MPD" {
        return Ok(None);
    }
    if mpd.attribute("type") == Some("dynamic") {
        bail!(t!("net.segment.live"));
    }

    let duration = mpd
        .attribute("mediaPresentationDuration")
        .and_then(parse_iso8601_duration);
    let mpd_base = resolve_base(mpd, base)?;
    let mut variants = Vec::new();

    for period in children(mpd, "Period") {
        let period_duration = period
            .attribute("duration")
            .and_then(parse_iso8601_duration)
            .or(duration);
        let period_base = resolve_base(period, &mpd_base)?;
        let period_template = Template::from_node(period);
        let period_list = child(period, "SegmentList");
        let period_base_segment = child(period, "SegmentBase");

        for adaptation in children(period, "AdaptationSet") {
            if has_drm(adaptation) {
                bail!(t!("net.segment.drm"));
            }
            let adaptation_base = resolve_base(adaptation, &period_base)?;
            let mime_type = adaptation
                .attribute("mimeType")
                .or_else(|| adaptation.attribute("contentType"))
                .map(str::to_owned);
            let inherited_template =
                Template::merged(period_template.as_ref(), Template::from_node(adaptation));
            let inherited_list = child(adaptation, "SegmentList").or(period_list);
            let inherited_base = child(adaptation, "SegmentBase").or(period_base_segment);

            for representation in children(adaptation, "Representation") {
                if has_drm(representation) {
                    bail!(t!("net.segment.drm"));
                }
                let rep_base = resolve_base(representation, &adaptation_base)?;
                let bandwidth = representation
                    .attribute("bandwidth")
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0);
                let id = representation
                    .attribute("id")
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("b{bandwidth}"));

                let template = Template::merged(
                    inherited_template.as_ref(),
                    Template::from_node(representation),
                );
                let plan = if let Some(template) = template {
                    plan_from_template(&template, &rep_base, period_duration, &id, bandwidth)?
                } else if let Some(list) = child(representation, "SegmentList").or(inherited_list) {
                    plan_from_list(list, &rep_base, period_duration, &id)?
                } else if let Some(base_segment) =
                    child(representation, "SegmentBase").or(inherited_base)
                {
                    plan_from_base(base_segment, &rep_base, period_duration, &id)?
                } else {
                    plan_from_file(&rep_base, period_duration, &id)?
                };

                if plan.segments.is_empty() {
                    bail!(t!("net.segment.empty"));
                }
                variants.push(DashVariant {
                    id,
                    bandwidth,
                    mime_type: mime_type.clone(),
                    plan,
                });
            }
        }
        if !variants.is_empty() {
            // Multi-period would need muxing across periods; first period only.
            break;
        }
    }

    if variants.is_empty() {
        bail!(t!("net.segment.empty"));
    }
    Ok(Some(variants))
}

fn plan_from_template(
    template: &Template,
    base: &Url,
    duration: Option<f64>,
    id: &str,
    bandwidth: u64,
) -> Result<SegmentPlan> {
    let media = template
        .media
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!(t!("net.segment.template", detail = "missing media URL")))?;
    let timescale = template.timescale.unwrap_or(1).max(1);
    let start_number = template.start_number.unwrap_or(1);
    let offset = template.presentation_time_offset.unwrap_or(0);
    let init = match template.initialization.as_deref() {
        Some(uri) => {
            let uri = expand(uri, start_number, offset, bandwidth, id)?;
            Some(resolve_ref(base, &uri, None)?)
        }
        None => None,
    };
    let mut segments = Vec::new();

    if template.timeline.is_empty() {
        let step = template.duration.ok_or_else(|| {
            anyhow::anyhow!(t!("net.segment.template", detail = "missing duration"))
        })?;
        if step == 0 {
            bail!(t!("net.segment.template", detail = "zero segment duration"));
        }
        let total = duration.ok_or_else(|| anyhow::anyhow!(t!("net.segment.duration")))?;
        let count = ((total * timescale as f64) / step as f64).ceil();
        if !count.is_finite() || count <= 0.0 {
            bail!(t!("net.segment.empty"));
        }
        if count > MAX_SEGMENTS as f64 {
            bail!(t!(
                "net.segment.template",
                detail = "template produces too many segments"
            ));
        }
        let count = count as usize;
        for index in 0..count {
            let number = start_number.saturating_add(index as u64);
            let time = offset.saturating_add((index as u64).saturating_mul(step));
            let uri = expand(media, number, time, bandwidth, id)?;
            segments.push(resolve_ref(base, &uri, None)?);
        }
    } else {
        // `S@t` is already the media time; the offset is not added again.
        for (number, time) in
            expand_timeline(&template.timeline, timescale, duration, start_number)?
        {
            let uri = expand(media, number, time, bandwidth, id)?;
            segments.push(resolve_ref(base, &uri, None)?);
        }
    }

    Ok(SegmentPlan {
        kind: SegmentKind::Dash,
        selected: Some(id.to_owned()),
        init,
        segments,
        duration_secs: duration,
    })
}

fn plan_from_list(list: Node, base: &Url, duration: Option<f64>, id: &str) -> Result<SegmentPlan> {
    let mut segments = Vec::new();
    for entry in children(list, "SegmentURL") {
        let media = entry.attribute("media").unwrap_or("").trim();
        if media.is_empty() {
            bail!(t!(
                "net.segment.template",
                detail = "SegmentURL without media"
            ));
        }
        let byte_range = match entry.attribute("mediaRange") {
            Some(value) => Some(parse_media_range(value)?),
            None => None,
        };
        segments.push(resolve_ref(base, media, byte_range)?);
    }

    let init = match child(list, "Initialization") {
        Some(node) => {
            let range = match node.attribute("range") {
                Some(value) => Some(parse_media_range(value)?),
                None => None,
            };
            match node.attribute("sourceURL") {
                Some(source) => Some(resolve_ref(base, source, range)?),
                None => range.map(|range| SegmentRef {
                    url: base.clone(),
                    byte_range: Some(range),
                }),
            }
        }
        None => None,
    };

    Ok(SegmentPlan {
        kind: SegmentKind::Dash,
        selected: Some(id.to_owned()),
        init,
        segments,
        duration_secs: duration,
    })
}

fn plan_from_base(
    base_segment: Node,
    base: &Url,
    duration: Option<f64>,
    id: &str,
) -> Result<SegmentPlan> {
    let init = match child(base_segment, "Initialization") {
        Some(node) => {
            let range = match node.attribute("range") {
                Some(value) => Some(parse_media_range(value)?),
                None => None,
            };
            match node.attribute("sourceURL") {
                Some(source) => Some(resolve_ref(base, source, range)?),
                // A bare range belongs to the same file; the full download includes it.
                None => None,
            }
        }
        None => None,
    };
    single_file(base, init, duration, id)
}

fn plan_from_file(base: &Url, duration: Option<f64>, id: &str) -> Result<SegmentPlan> {
    single_file(base, None, duration, id)
}

fn single_file(
    base: &Url,
    init: Option<SegmentRef>,
    duration: Option<f64>,
    id: &str,
) -> Result<SegmentPlan> {
    if base.path().ends_with('/') {
        bail!(t!(
            "net.segment.template",
            detail = "representation has no BaseURL file"
        ));
    }
    Ok(SegmentPlan {
        kind: SegmentKind::Dash,
        selected: Some(id.to_owned()),
        init,
        segments: vec![SegmentRef {
            url: base.clone(),
            byte_range: None,
        }],
        duration_secs: duration,
    })
}

/// `#EXT-X-BYTERANGE`-style but DASH uses `start-end` (both inclusive).
fn parse_media_range(value: &str) -> Result<ByteRange> {
    let (start, end) = value.trim().split_once('-').ok_or_else(|| {
        anyhow::anyhow!(t!(
            "net.segment.invalid",
            detail = format!("bad range {value:?}")
        ))
    })?;
    let start: u64 = start.trim().parse().map_err(|_| {
        anyhow::anyhow!(t!(
            "net.segment.invalid",
            detail = format!("bad range {value:?}")
        ))
    })?;
    let end: u64 = end.trim().parse().map_err(|_| {
        anyhow::anyhow!(t!(
            "net.segment.invalid",
            detail = format!("bad range {value:?}")
        ))
    })?;
    if end < start {
        bail!(t!(
            "net.segment.invalid",
            detail = format!("bad range {value:?}")
        ));
    }
    Ok(ByteRange {
        start,
        end: Some(end),
    })
}

fn resolve_ref(base: &Url, uri: &str, byte_range: Option<ByteRange>) -> Result<SegmentRef> {
    let uri = uri.trim();
    if uri.is_empty() {
        bail!(t!("net.segment.invalid", detail = "empty URL"));
    }
    Ok(SegmentRef {
        url: base
            .join(uri)
            .with_context(|| t!("net.segment.invalid", detail = format!("bad URL {uri:?}")))?,
        byte_range,
    })
}

#[derive(Debug, Clone, Default)]
struct Template {
    media: Option<String>,
    initialization: Option<String>,
    timescale: Option<u64>,
    duration: Option<u64>,
    start_number: Option<u64>,
    presentation_time_offset: Option<u64>,
    timeline: Vec<TimelineEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimelineEntry {
    time: Option<u64>,
    duration: u64,
    repeat: i64,
}

impl Template {
    fn from_node(node: Node) -> Option<Self> {
        let template = child(node, "SegmentTemplate")?;
        Some(Self {
            media: template.attribute("media").map(str::to_owned),
            initialization: template.attribute("initialization").map(str::to_owned),
            timescale: number_attr(template, "timescale"),
            duration: number_attr(template, "duration"),
            start_number: number_attr(template, "startNumber"),
            presentation_time_offset: number_attr(template, "presentationTimeOffset"),
            timeline: parse_timeline(template),
        })
    }

    fn merged(parent: Option<&Self>, own: Option<Self>) -> Option<Self> {
        match (parent, own) {
            (None, None) => None,
            (Some(parent), None) => Some(parent.clone()),
            (None, Some(own)) => Some(own),
            (Some(parent), Some(own)) => Some(Self {
                media: own.media.or_else(|| parent.media.clone()),
                initialization: own.initialization.or_else(|| parent.initialization.clone()),
                timescale: own.timescale.or(parent.timescale),
                duration: own.duration.or(parent.duration),
                start_number: own.start_number.or(parent.start_number),
                presentation_time_offset: own
                    .presentation_time_offset
                    .or(parent.presentation_time_offset),
                timeline: if own.timeline.is_empty() {
                    parent.timeline.clone()
                } else {
                    own.timeline
                },
            }),
        }
    }
}

fn parse_timeline(template: Node) -> Vec<TimelineEntry> {
    let Some(timeline) = child(template, "SegmentTimeline") else {
        return Vec::new();
    };
    children(timeline, "S")
        .filter_map(|entry| {
            Some(TimelineEntry {
                time: number_attr(entry, "t"),
                duration: number_attr(entry, "d")?,
                repeat: entry
                    .attribute("r")
                    .and_then(|value| value.trim().parse().ok())
                    .unwrap_or(0),
            })
        })
        .collect()
}

fn expand_timeline(
    entries: &[TimelineEntry],
    timescale: u64,
    duration: Option<f64>,
    start_number: u64,
) -> Result<Vec<(u64, u64)>> {
    let mut segments = Vec::new();
    let mut number = start_number;
    let mut next_time = 0u64;
    for entry in entries {
        if entry.duration == 0 {
            bail!(t!(
                "net.segment.template",
                detail = "zero timeline duration"
            ));
        }
        let start = entry.time.unwrap_or(next_time);
        let repeats = if entry.repeat < 0 {
            let end = duration
                .map(|secs| (secs * timescale as f64).ceil() as u64)
                .unwrap_or_else(|| start.saturating_add(entry.duration));
            let count = end.saturating_sub(start) / entry.duration;
            count.saturating_sub(1) as i64
        } else {
            entry.repeat
        };
        let mut time = start;
        for _ in 0..=repeats {
            if segments.len() >= MAX_SEGMENTS {
                bail!(t!(
                    "net.segment.template",
                    detail = "segment timeline too long"
                ));
            }
            segments.push((number, time));
            number = number.saturating_add(1);
            time = time.saturating_add(entry.duration);
        }
        next_time = time;
    }
    if segments.is_empty() {
        bail!(t!("net.segment.empty"));
    }
    Ok(segments)
}

/// Expands `$Number$`, `$Time$`, `$Bandwidth$`, `$RepresentationID$` (and
/// printf-style widths such as `$Number%05d$`).
fn expand(
    template: &str,
    number: u64,
    time: u64,
    bandwidth: u64,
    representation: &str,
) -> Result<String> {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('$') else {
            bail!(t!(
                "net.segment.template",
                detail = "unterminated placeholder"
            ));
        };
        let token = &after[..end];
        if token.is_empty() {
            out.push('$');
        } else {
            let (name, format) = match token.split_once('%') {
                Some((name, format)) => (name, Some(format)),
                None => (token, None),
            };
            if name == "RepresentationID" {
                out.push_str(representation);
            } else {
                let value = match name {
                    "Number" => number,
                    "Time" => time,
                    "Bandwidth" => bandwidth,
                    _ => bail!(t!(
                        "net.segment.template",
                        detail = format!("unknown placeholder ${name}$")
                    )),
                };
                match format {
                    Some(format) => out.push_str(&format_number(value, format)?),
                    None => out.push_str(&value.to_string()),
                }
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn format_number(value: u64, format: &str) -> Result<String> {
    let (padded, digits) = match format.strip_prefix('0') {
        Some(digits) => (true, digits),
        None => (false, format),
    };
    let width = digits
        .strip_suffix('d')
        .and_then(|digits| digits.parse::<usize>().ok())
        .filter(|width| *width <= 64)
        .ok_or_else(|| {
            anyhow::anyhow!(t!(
                "net.segment.template",
                detail = format!("bad placeholder format %{format}")
            ))
        })?;
    Ok(if padded {
        format!("{value:0width$}")
    } else {
        format!("{value:width$}")
    })
}

/// ISO 8601 duration (`PT1H2M3.5S`, `P1DT...`) in seconds. Years and months
/// are rejected: they have no fixed length and never describe media segments.
fn parse_iso8601_duration(value: &str) -> Option<f64> {
    let value = value.trim();
    let rest = value.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total = 0.0;
    let mut number = String::new();
    for ch in date.chars() {
        if ch.is_ascii_digit() || ch == '.' || ch == ',' {
            number.push(if ch == ',' { '.' } else { ch });
            continue;
        }
        let amount: f64 = number.parse().ok()?;
        number.clear();
        total += match ch {
            'D' => amount * 86_400.0,
            'W' => amount * 604_800.0,
            _ => return None,
        };
    }
    if !number.is_empty() {
        return None;
    }
    for ch in time.chars() {
        if ch.is_ascii_digit() || ch == '.' || ch == ',' {
            number.push(if ch == ',' { '.' } else { ch });
            continue;
        }
        let amount: f64 = number.parse().ok()?;
        number.clear();
        total += match ch {
            'H' => amount * 3_600.0,
            'M' => amount * 60.0,
            'S' => amount,
            _ => return None,
        };
    }
    if !number.is_empty() {
        return None;
    }
    Some(total)
}

fn resolve_base(node: Node, parent: &Url) -> Result<Url> {
    let mut base = parent.clone();
    for child in children(node, "BaseURL") {
        let text = child.text().unwrap_or("").trim();
        if !text.is_empty() {
            base = base.join(text).with_context(|| {
                t!(
                    "net.segment.invalid",
                    detail = format!("bad BaseURL {text:?}")
                )
            })?;
        }
    }
    Ok(base)
}

fn has_drm(node: Node) -> bool {
    children(node, "ContentProtection").next().is_some()
}

fn number_attr(node: Node, name: &str) -> Option<u64> {
    node.attribute(name)
        .and_then(|value| value.trim().parse().ok())
}

fn children<'a, 'input, 'n>(
    node: Node<'a, 'input>,
    name: &'n str,
) -> impl Iterator<Item = Node<'a, 'input>> + 'n
where
    'a: 'n,
    'input: 'n,
{
    node.children()
        .filter(move |child| child.is_element() && child.tag_name().name() == name)
}

fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    children(node, name).next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.test/movie/manifest.mpd").unwrap()
    }

    fn template_mpd() -> &'static str {
        r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT6S">
  <BaseURL>https://cdn.test/v/</BaseURL>
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <SegmentTemplate timescale="1" duration="2" startNumber="1"
                       initialization="init.mp4" media="seg-$Number%03d$.m4s"/>
      <Representation id="low" bandwidth="800000"/>
      <Representation id="high" bandwidth="2400000"/>
    </AdaptationSet>
  </Period>
</MPD>"#
    }

    #[test]
    fn ignores_foreign_documents() {
        assert!(parse(&base(), "<html/>").unwrap().is_none());
    }

    #[test]
    fn expands_number_template_for_best_representation() {
        let variants = parse(&base(), template_mpd()).unwrap().unwrap();
        assert_eq!(variants.len(), 2);
        let best = DashVariant::best(&variants).unwrap();
        assert_eq!(best.id, "high");
        assert_eq!(best.mime_type.as_deref(), Some("video/mp4"));
        assert_eq!(best.plan.segments.len(), 3);
        assert_eq!(
            best.plan.segments[0].url.as_str(),
            "https://cdn.test/v/seg-001.m4s"
        );
        assert_eq!(
            best.plan.segments[2].url.as_str(),
            "https://cdn.test/v/seg-003.m4s"
        );
        assert_eq!(
            best.plan.init.as_ref().unwrap().url.as_str(),
            "https://cdn.test/v/init.mp4"
        );
        assert_eq!(best.plan.duration_secs, Some(6.0));
        assert_eq!(best.plan.kind, SegmentKind::Dash);
    }

    #[test]
    fn expands_segment_timeline_with_time_placeholder() {
        let mpd = r#"<MPD mediaPresentationDuration="PT6S">
  <Period>
    <AdaptationSet>
      <Representation id="v" bandwidth="1000">
        <BaseURL>https://cdn.test/t/</BaseURL>
        <SegmentTemplate timescale="1000" media="t$Time$.m4s" initialization="i.mp4">
          <SegmentTimeline>
            <S t="0" d="2000" r="1"/>
            <S d="1000"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let variants = parse(&base(), mpd).unwrap().unwrap();
        let plan = &variants[0].plan;
        let urls: Vec<&str> = plan.segments.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://cdn.test/t/t0.m4s",
                "https://cdn.test/t/t2000.m4s",
                "https://cdn.test/t/t4000.m4s"
            ]
        );
        assert_eq!(
            plan.init.as_ref().unwrap().url.as_str(),
            "https://cdn.test/t/i.mp4"
        );
    }

    #[test]
    fn inherited_base_url_and_representation_id() {
        let mpd = r#"<MPD mediaPresentationDuration="PT4S">
  <Period>
    <AdaptationSet>
      <BaseURL>https://cdn.test/a/</BaseURL>
      <Representation id="r1" bandwidth="5">
        <BaseURL>r1/</BaseURL>
        <SegmentTemplate timescale="1" duration="2" media="$RepresentationID$-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let variants = parse(&base(), mpd).unwrap().unwrap();
        assert_eq!(
            variants[0].plan.segments[1].url.as_str(),
            "https://cdn.test/a/r1/r1-2.m4s"
        );
    }

    #[test]
    fn initialization_expands_representation_id() {
        let mpd = r#"<MPD mediaPresentationDuration="PT2S">
  <Period><AdaptationSet><Representation id="v7" bandwidth="5">
    <BaseURL>https://cdn.test/x/</BaseURL>
    <SegmentTemplate timescale="1" duration="2" media="s$Number$.m4s"
                     initialization="$RepresentationID$/init.mp4"/>
  </Representation></AdaptationSet></Period>
</MPD>"#;
        let variants = parse(&base(), mpd).unwrap().unwrap();
        assert_eq!(
            variants[0].plan.init.as_ref().unwrap().url.as_str(),
            "https://cdn.test/x/v7/init.mp4"
        );
    }

    #[test]
    fn segment_list_with_ranges_and_init() {
        let mpd = r#"<MPD mediaPresentationDuration="PT2S">
  <Period>
    <AdaptationSet>
      <Representation id="v" bandwidth="1">
        <BaseURL>https://cdn.test/s/file.mp4</BaseURL>
        <SegmentList>
          <Initialization sourceURL="init.mp4" range="0-99"/>
          <SegmentURL media="file.mp4" mediaRange="100-199"/>
          <SegmentURL media="file.mp4" mediaRange="200-299"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let variants = parse(&base(), mpd).unwrap().unwrap();
        let plan = &variants[0].plan;
        assert_eq!(
            plan.init.as_ref().unwrap().url.as_str(),
            "https://cdn.test/s/init.mp4"
        );
        assert_eq!(
            plan.init.as_ref().unwrap().byte_range,
            Some(ByteRange {
                start: 0,
                end: Some(99)
            })
        );
        assert_eq!(
            plan.segments[0].byte_range,
            Some(ByteRange {
                start: 100,
                end: Some(199)
            })
        );
        assert_eq!(plan.segments[1].url.as_str(), "https://cdn.test/s/file.mp4");
    }

    #[test]
    fn segment_base_downloads_single_file() {
        let mpd = r#"<MPD mediaPresentationDuration="PT2S">
  <Period><AdaptationSet><Representation id="v" bandwidth="1">
    <BaseURL>https://cdn.test/solo.mp4</BaseURL>
    <SegmentBase indexRange="0-50"/>
  </Representation></AdaptationSet></Period>
</MPD>"#;
        let variants = parse(&base(), mpd).unwrap().unwrap();
        assert_eq!(variants[0].plan.segments.len(), 1);
        assert_eq!(
            variants[0].plan.segments[0].url.as_str(),
            "https://cdn.test/solo.mp4"
        );
    }

    #[test]
    fn rejects_dynamic_and_drm() {
        let dynamic = r#"<MPD type="dynamic"><Period/></MPD>"#;
        assert!(parse(&base(), dynamic).is_err());

        let drm = r#"<MPD mediaPresentationDuration="PT2S">
  <Period><AdaptationSet>
    <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011"/>
    <Representation id="v" bandwidth="1"><BaseURL>f.mp4</BaseURL></Representation>
  </AdaptationSet></Period>
</MPD>"#;
        assert!(parse(&base(), drm).is_err());
    }

    #[test]
    fn rejects_missing_duration_without_timeline() {
        let mpd = r#"<MPD><Period><AdaptationSet><Representation id="v" bandwidth="1">
  <SegmentTemplate timescale="1" duration="2" media="s$Number$.m4s"/>
</Representation></AdaptationSet></Period></MPD>"#;
        assert!(parse(&base(), mpd).is_err());
    }

    #[test]
    fn iso_durations() {
        assert_eq!(parse_iso8601_duration("PT6S"), Some(6.0));
        assert_eq!(parse_iso8601_duration("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(parse_iso8601_duration("P1DT1S"), Some(86401.0));
        assert_eq!(parse_iso8601_duration("PT0.5S"), Some(0.5));
        assert_eq!(parse_iso8601_duration("P1M"), None);
        assert_eq!(parse_iso8601_duration("nope"), None);
    }

    #[test]
    fn placeholder_formats() {
        assert_eq!(
            expand("s$Number%05d$.m4s", 7, 0, 0, "r").unwrap(),
            "s00007.m4s"
        );
        assert_eq!(expand("$Number$", 7, 0, 0, "r").unwrap(), "7");
        assert_eq!(expand("cost$$", 1, 0, 0, "r").unwrap(), "cost$");
        assert!(expand("$Bogus$", 1, 0, 0, "r").is_err());
    }
}
