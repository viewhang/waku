//! Source-preserving Markdown operations shared by the assistant and the native library editor

use anyhow::{Context, Result, ensure};
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};
use serde_json::{Map, Value};
use std::{collections::HashSet, path::Path};
use url::Url;
use waku_protocol::knowledge::TagMode;

/// Parse the front matter separately so mutations never interpret the note body as YAML
pub(super) fn front_matter(text: &str) -> Result<(Map<String, Value>, &str)> {
    let Some((header, body)) = waku_protocol::knowledge::split_front_matter(text) else {
        return Ok((Map::new(), text));
    };
    let fields = if header.trim().is_empty() {
        Map::new()
    } else {
        serde_saphyr::from_str(header)?
    };
    Ok((fields, body))
}

pub(super) fn metadata(text: &str, extra: Map<String, Value>) -> Result<String> {
    let (mut fields, body) = front_matter(text)?;
    fields.extend(extra);
    Ok(format!(
        "---\n{}---\n{body}",
        serde_saphyr::to_string(&fields)?
    ))
}

pub(super) fn tag(text: &str, tags: &[String], op: &TagMode) -> Result<String> {
    let (fields, _) = front_matter(text)?;
    let mut current: Vec<String> = match fields.get("tags") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(other) => {
            serde_json::from_value(other.clone()).context("tags must be a string or string list")?
        }
        None => Vec::new(),
    };
    let before = current.clone();
    for tag in tags {
        let tag = tag.trim();
        ensure!(!tag.is_empty(), "A tag cannot be empty");
        match op {
            TagMode::Add if !current.iter().any(|t| t == tag) => current.push(tag.into()),
            TagMode::Remove => current.retain(|t| t != tag),
            _ => {}
        }
    }
    if before == current {
        return Ok(text.to_owned());
    }
    metadata(
        text,
        Map::from_iter([("tags".into(), serde_json::to_value(current)?)]),
    )
}

/// Rebase Markdown destinations with file URLs so spaces, fragments and relative directories survive a move
pub(super) fn moved_links(
    text: &str,
    root: &Path,
    owner: &str,
    from: &str,
    to: &str,
) -> Result<String> {
    let source = Url::from_file_path(root.join(owner))
        .map_err(|_| anyhow::anyhow!("Invalid source path"))?;
    let new_owner = if owner == from { to } else { owner };
    let base = Url::from_file_path(root.join(new_owner))
        .map_err(|_| anyhow::anyhow!("Invalid destination path"))?;
    let old = root.join(from);
    let new = root.join(to);
    let parser = Parser::new_ext(text, Options::all());
    let mut ranges = Vec::new();
    for (_, def) in parser.reference_definitions().iter() {
        ranges.push((def.span.clone(), def.dest.to_string(), true));
    }
    for (event, r) in parser.into_offset_iter() {
        if let Event::Start(
            Tag::Link {
                link_type: LinkType::Inline,
                dest_url,
                ..
            }
            | Tag::Image {
                link_type: LinkType::Inline,
                dest_url,
                ..
            },
        ) = event
        {
            ranges.push((r, dest_url.to_string(), false));
        }
    }
    let mut edits = Vec::new();
    let mut seen = HashSet::new();
    for (r, dest, reference) in ranges {
        if dest.starts_with(['#', '/']) || Url::parse(&dest).is_ok() {
            continue;
        }
        let Ok(mut target) = source.join(&dest) else {
            continue;
        };
        let Ok(path) = target.to_file_path() else {
            continue;
        };
        if path == old {
            let mut replacement = Url::from_file_path(&new).unwrap();
            replacement.set_fragment(target.fragment());
            replacement.set_query(target.query());
            target = replacement;
        } else if owner == new_owner {
            continue;
        }
        let Some(relative) = base.make_relative(&target) else {
            continue;
        };
        let raw = &text[r.clone()];
        let after = if reference {
            raw.find("]:").map(|i| i + 2)
        } else {
            raw.rfind("](").map(|i| i + 2)
        };
        let Some(after) = after else { continue };
        let tail = &raw[after..];
        let stripped = tail.trim_start().trim_start_matches('<');
        if stripped.starts_with(&dest) {
            let start = r.start + after + tail.len() - stripped.len();
            if seen.insert(start) {
                edits.push((start..start + dest.len(), relative));
            }
        }
    }
    edits.sort_by_key(|(r, _)| r.start);
    let mut out = text.to_owned();
    for (r, dest) in edits.into_iter().rev() {
        out.replace_range(r, &dest);
    }
    Ok(out)
}
