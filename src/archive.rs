//! Search the archive.org Live Music Archive (the `etree` collection) for
//! shows that have MP3 derivatives, and therefore a streamable `_vbr.m3u`.

use anyhow::Context;
use serde::Deserialize;
use url::Url;

use crate::playlist::json_string;

pub const PAGE_SIZE: usize = 50;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    /// Newest show first.
    Date,
    /// Most downloaded first.
    Popular,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Show {
    pub identifier: String,
    pub title: String,
    pub creator: String,
    /// `YYYY-MM-DD`, or empty.
    pub date: String,
    pub venue: String,
    pub coverage: String,
    pub downloads: u64,
    pub rating: Option<f32>,
}

impl Show {
    pub fn m3u_url(&self) -> Url {
        m3u_url(&self.identifier)
    }

    /// One-line summary for a list row: date, band, venue and city.
    pub fn line(&self) -> String {
        let mut s = String::new();
        if !self.date.is_empty() {
            s.push_str(&self.date);
            s.push_str("  ");
        }
        if !self.creator.is_empty() {
            s.push_str(&self.creator);
            s.push_str("  -  ");
        }
        let place = match (self.venue.is_empty(), self.coverage.is_empty()) {
            (false, false) => format!("{}, {}", self.venue, self.coverage),
            (false, true) => self.venue.clone(),
            (true, false) => self.coverage.clone(),
            (true, true) => self.title.clone(),
        };
        s.push_str(&place);
        s
    }
}

#[derive(Clone, Debug, Default)]
pub struct SearchPage {
    pub shows: Vec<Show>,
    pub total: usize,
    pub page: usize,
}

/// archive.org derives `<identifier>_vbr.m3u` for every item with VBR MP3s.
pub fn m3u_url(identifier: &str) -> Url {
    Url::parse(&format!("https://archive.org/download/{identifier}/{identifier}_vbr.m3u"))
        .expect("identifier is url-safe")
}

/// Build the Lucene-style query. Free text matches the artist or the title;
/// an empty query lists everything (newest/most popular first).
pub fn build_query(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if "\"():[]{}^~\\/+-!&|*?".contains(c) { ' ' } else { c })
        .collect();
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    let base = "collection:etree AND format:\"VBR MP3\"";
    if words.is_empty() {
        return base.to_string();
    }
    let phrase = words.join(" ");
    format!("{base} AND (creator:\"{phrase}\" OR creator:({phrase}) OR title:({phrase}))")
}

#[derive(Deserialize)]
struct Envelope {
    response: Response,
}

#[derive(Deserialize)]
struct Response {
    #[serde(rename = "numFound", default)]
    num_found: usize,
    #[serde(default)]
    docs: Vec<serde_json::Value>,
}

pub async fn search(client: &reqwest::Client, text: &str, sort: Sort, page: usize) -> anyhow::Result<SearchPage> {
    let sort_param = match sort {
        Sort::Date => "date desc",
        Sort::Popular => "downloads desc",
    };
    let fields = ["identifier", "title", "date", "venue", "coverage", "creator", "downloads", "avg_rating"];
    let mut params: Vec<(&str, String)> = vec![
        ("q", build_query(text)),
        ("sort[]", sort_param.to_string()),
        ("rows", PAGE_SIZE.to_string()),
        ("page", (page + 1).to_string()),
        ("output", "json".to_string()),
    ];
    for f in fields {
        params.push(("fl[]", f.to_string()));
    }
    let url = Url::parse_with_params("https://archive.org/advancedsearch.php", &params)
        .context("bad search parameters")?;
    let body = client
        .get(url)
        .send()
        .await
        .context("search request failed")?
        .error_for_status()
        .context("archive.org rejected the search")?
        .text()
        .await
        .context("could not read search response")?;
    let env: Envelope = serde_json::from_str(&body).context("unexpected search response")?;
    let shows = env.response.docs.iter().filter_map(parse_doc).collect();
    Ok(SearchPage { shows, total: env.response.num_found, page })
}

fn parse_doc(doc: &serde_json::Value) -> Option<Show> {
    let identifier = json_string(&doc["identifier"])?;
    let date = json_string(&doc["date"]).map(|d| d.chars().take(10).collect()).unwrap_or_default();
    Some(Show {
        title: json_string(&doc["title"]).unwrap_or_else(|| identifier.clone()),
        creator: json_string(&doc["creator"]).unwrap_or_default(),
        date,
        venue: json_string(&doc["venue"]).unwrap_or_default(),
        coverage: json_string(&doc["coverage"]).unwrap_or_default(),
        downloads: doc["downloads"].as_u64().unwrap_or(0),
        rating: doc["avg_rating"].as_f64().map(|r| r as f32),
        identifier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_building() {
        assert_eq!(build_query("  "), "collection:etree AND format:\"VBR MP3\"");
        let q = build_query("Billy \"Strings\" (live)");
        assert!(q.starts_with("collection:etree AND format:\"VBR MP3\" AND ("));
        assert!(q.contains("creator:\"Billy Strings live\""));
        assert!(q.contains("title:(Billy Strings live)"), "{q}");
    }

    #[test]
    fn doc_parsing_and_line() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"identifier":"x2026","title":"X Live","date":"2026-09-26T00:00:00Z","venue":"The Forum","coverage":"Inglewood, CA","creator":["X"],"downloads":12,"avg_rating":4.5}"#,
        )
        .unwrap();
        let show = parse_doc(&doc).unwrap();
        assert_eq!(show.date, "2026-09-26");
        assert_eq!(show.creator, "X");
        assert_eq!(show.rating, Some(4.5));
        assert_eq!(show.line(), "2026-09-26  X  -  The Forum, Inglewood, CA");
        assert_eq!(show.m3u_url().as_str(), "https://archive.org/download/x2026/x2026_vbr.m3u");
    }
}
