//! M3U / EXTM3U fetching and parsing, plus optional metadata enrichment for
//! archive.org items (real song titles, lengths).

use anyhow::{Context, bail};
use serde::Deserialize;
use url::Url;

#[derive(Clone, Debug)]
pub struct Track {
    pub url: Url,
    pub title: String,
    pub duration_secs: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Playlist {
    /// Human-readable name (item title for archive.org, file stem otherwise).
    pub name: String,
    /// Artist / creator, if known.
    pub artist: Option<String>,
    pub tracks: Vec<Track>,
}

impl Playlist {
    pub fn total_secs(&self) -> u32 {
        self.tracks.iter().filter_map(|t| t.duration_secs).sum()
    }
}

/// Parse M3U text. Relative entries are resolved against `base`.
pub fn parse_m3u(base: &Url, text: &str) -> Vec<Track> {
    let mut tracks = Vec::new();
    let mut pending: Option<(Option<u32>, String)> = None;

    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let (dur, title) = rest.split_once(',').unwrap_or((rest, ""));
            let secs = dur
                .split_whitespace()
                .next()
                .and_then(|d| d.parse::<f64>().ok())
                .filter(|d| *d > 0.0)
                .map(|d| d.round() as u32);
            pending = Some((secs, title.trim().to_string()));
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let Ok(url) = base.join(line) else { continue };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        let (secs, title) = pending.take().unwrap_or((None, String::new()));
        let title = if title.is_empty() { title_from_url(&url) } else { title };
        tracks.push(Track { url, title, duration_secs: secs });
    }
    tracks
}

/// Last path segment, percent-decoded, without extension.
pub fn title_from_url(url: &Url) -> String {
    let last = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or("");
    let decoded = percent_decode(last);
    let stem = match decoded.rsplit_once('.') {
        Some((s, ext)) if !s.is_empty() && ext.len() <= 4 => s.to_string(),
        _ => decoded,
    };
    if stem.is_empty() { url.to_string() } else { stem }
}

pub fn file_name(url: &Url) -> String {
    percent_decode(
        url.path_segments()
            .and_then(|mut s| s.next_back())
            .unwrap_or(""),
    )
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &s[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Download and parse a playlist, then try to enrich it with metadata.
pub async fn fetch(client: &reqwest::Client, url: Url) -> anyhow::Result<Playlist> {
    let resp = client
        .get(url.clone())
        .send()
        .await
        .context("request failed")?
        .error_for_status()
        .context("server rejected request")?;
    let final_url = resp.url().clone();
    let text = resp.text().await.context("could not read playlist body")?;

    let tracks = parse_m3u(&final_url, &text);
    if tracks.is_empty() {
        bail!("no playable http(s) entries found in playlist");
    }

    let mut playlist = Playlist {
        name: title_from_url(&url),
        artist: None,
        tracks,
    };

    if let Some(id) = archive_identifier(&final_url)
        && let Ok(meta) = fetch_archive_metadata(client, &id).await
    {
        apply_archive_metadata(&mut playlist, &meta);
    }

    Ok(playlist)
}

/// For `https://archive.org/download/<identifier>/...` return `<identifier>`.
fn archive_identifier(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    if host != "archive.org" && !host.ends_with(".archive.org") {
        return None;
    }
    let mut segs = url.path_segments()?;
    let first = segs.next()?;
    let id = segs.next()?;
    // Direct download links redirect to `/<n>/items/<identifier>/...` on a node.
    match first {
        "download" => Some(id.to_string()),
        _ => {
            let mut segs = url.path_segments()?;
            while let Some(s) = segs.next() {
                if s == "items" {
                    return segs.next().map(str::to_string);
                }
            }
            None
        }
    }
}

#[derive(Deserialize, Default)]
struct ArchiveMeta {
    #[serde(default)]
    files: Vec<ArchiveFile>,
    #[serde(default)]
    metadata: serde_json::Value,
}

#[derive(Deserialize)]
struct ArchiveFile {
    name: String,
    title: Option<String>,
    length: Option<String>,
}

async fn fetch_archive_metadata(client: &reqwest::Client, id: &str) -> anyhow::Result<ArchiveMeta> {
    let url = format!("https://archive.org/metadata/{id}");
    let body = client.get(url).send().await?.error_for_status()?.text().await?;
    Ok(serde_json::from_str(&body)?)
}

fn apply_archive_metadata(playlist: &mut Playlist, meta: &ArchiveMeta) {
    if let Some(t) = json_string(&meta.metadata["title"]) {
        playlist.name = t;
    }
    playlist.artist = json_string(&meta.metadata["creator"]);

    for track in &mut playlist.tracks {
        let name = file_name(&track.url);
        let Some(file) = meta.files.iter().find(|f| f.name == name) else {
            continue;
        };
        if let Some(t) = file.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            track.title = t.to_string();
        }
        if track.duration_secs.is_none() {
            track.duration_secs = file.length.as_deref().and_then(parse_length);
        }
    }
}

/// archive.org lengths come as "334.51" or "04:22" or "1:02:03".
fn parse_length(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.contains(':') {
        let mut total = 0f64;
        for part in s.split(':') {
            total = total * 60.0 + part.trim().parse::<f64>().ok()?;
        }
        return Some(total.round() as u32).filter(|v| *v > 0);
    }
    s.parse::<f64>().ok().map(|v| v.round() as u32).filter(|v| *v > 0)
}

/// archive.org metadata values may be a string or an array of strings.
fn json_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(a) => a.iter().find_map(json_string),
        _ => None,
    }
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

pub fn fmt_time(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_ext_m3u() {
        let base = Url::parse("https://example.com/shows/list.m3u").unwrap();
        let text = "#EXTM3U\n#EXTINF:123,Artist - Song\nhttps://cdn.example.com/a.mp3\n\n# comment\nb%20two.mp3\nrelative/c.ogg\nfile:///etc/passwd\n";
        let tracks = parse_m3u(&base, text);
        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].title, "Artist - Song");
        assert_eq!(tracks[0].duration_secs, Some(123));
        assert_eq!(tracks[1].url.as_str(), "https://example.com/shows/b%20two.mp3");
        assert_eq!(tracks[1].title, "b two");
        assert_eq!(tracks[2].url.as_str(), "https://example.com/shows/relative/c.ogg");
    }

    #[test]
    fn archive_ids() {
        let u = Url::parse("https://archive.org/download/Some-Item_2026/x.m3u").unwrap();
        assert_eq!(archive_identifier(&u).as_deref(), Some("Some-Item_2026"));
        let u = Url::parse("https://ia600800.us.archive.org/25/items/Some-Item/x.m3u").unwrap();
        assert_eq!(archive_identifier(&u).as_deref(), Some("Some-Item"));
        let u = Url::parse("https://example.com/download/x/x.m3u").unwrap();
        assert!(archive_identifier(&u).is_none());
    }

    #[test]
    fn lengths() {
        assert_eq!(parse_length("04:22"), Some(262));
        assert_eq!(parse_length("334.51"), Some(335));
        assert_eq!(parse_length("1:02:03"), Some(3723));
        assert_eq!(parse_length("nope"), None);
    }

    #[test]
    fn time_format() {
        assert_eq!(fmt_time(65), "1:05");
        assert_eq!(fmt_time(3723), "1:02:03");
    }
}
