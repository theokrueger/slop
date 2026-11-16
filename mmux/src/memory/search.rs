//! Streaming search over plaintext documents. No index: the answer must be
//! good enough that not having one is invisible at this scale.

use crate::memory::doc::Doc;

/// A parsed query: `"quoted phrase"` matches verbatim, bare terms are ANDed,
/// and a leading `-` excludes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub phrases: Vec<String>,
    pub terms: Vec<String>,
    pub excludes: Vec<String>,
}

impl Query {
    pub fn parse(raw: &str) -> Self {
        let mut query = Query::default();
        let chars = raw.chars();
        let mut word = String::new();
        let mut quoted = false;
        for ch in chars {
            match ch {
                '"' => {
                    if quoted {
                        push_phrase(&mut query, word.trim());
                        word.clear();
                    } else if !word.trim().is_empty() {
                        push_term(&mut query, word.trim());
                        word.clear();
                    }
                    quoted = !quoted;
                }
                ch if ch.is_whitespace() && !quoted => {
                    if !word.trim().is_empty() {
                        push_term(&mut query, word.trim());
                    }
                    word.clear();
                }
                ch => word.push(ch),
            }
            if word.len() > 200 {
                break;
            }
        }
        if !word.trim().is_empty() {
            if quoted {
                push_phrase(&mut query, word.trim());
            } else {
                push_term(&mut query, word.trim());
            }
        }
        query
    }

    pub fn is_empty(&self) -> bool {
        self.phrases.is_empty() && self.terms.is_empty() && self.excludes.is_empty()
    }
}

fn push_phrase(query: &mut Query, phrase: &str) {
    if !phrase.is_empty() {
        query.phrases.push(phrase.to_lowercase());
    }
}

fn push_term(query: &mut Query, term: &str) {
    if let Some(rest) = term.strip_prefix('-')
        && !rest.is_empty()
    {
        query.excludes.push(rest.to_lowercase());
        return;
    }
    if !term.is_empty() {
        query.terms.push(term.to_lowercase());
    }
}

#[derive(Debug, Clone)]
pub struct Scored {
    pub doc: Doc,
    pub score: i64,
    pub snippet: String,
}

/// Scores one document, or returns `None` when it does not match.
pub fn score(doc: &Doc, query: &Query) -> Option<(i64, String)> {
    let title = doc.title.to_lowercase();
    let tags = doc.tags.join(" ").to_lowercase();
    let body = doc.body.to_lowercase();
    let combined = format!("{title} {tags} {body}");

    for excluded in &query.excludes {
        if combined.contains(excluded.as_str()) {
            return None;
        }
    }

    let mut score = doc.kind.weight();
    let mut first_hit: Option<usize> = None;

    for phrase in &query.phrases {
        if !combined.contains(phrase.as_str()) {
            return None;
        }
        score += 4;
        if first_hit.is_none() {
            first_hit = body
                .find(phrase.as_str())
                .or_else(|| title.find(phrase.as_str()));
        }
    }

    for term in &query.terms {
        if !combined.contains(term.as_str()) {
            return None;
        }
        score += 1;
        if title.contains(term.as_str()) {
            score += 10;
        }
        if tags.contains(term.as_str()) {
            score += 6;
        }
        if first_hit.is_none() {
            first_hit = body
                .find(term.as_str())
                .or_else(|| title.find(term.as_str()));
        }
    }

    score += recency_bonus(&doc.updated);

    let snippet = match first_hit {
        Some(index) => excerpt(&doc.body, index),
        None => excerpt(&doc.body, 0),
    };
    Some((score, snippet))
}

fn recency_bonus(updated: &str) -> i64 {
    let Ok(stamp) =
        time::OffsetDateTime::parse(updated, &time::format_description::well_known::Rfc3339)
    else {
        return 0;
    };
    let days = (time::OffsetDateTime::now_utc() - stamp).whole_days();
    if days <= 0 { 30 } else { (30 - days).max(0) }
}

/// A small window around a byte offset, on character boundaries, with
/// whitespace collapsed so it fits one line of a result table.
pub(crate) fn excerpt(body: &str, offset: usize) -> String {
    const RADIUS: usize = 80;
    let mut start = offset.saturating_sub(RADIUS);
    while start > 0 && !body.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (offset + RADIUS).min(body.len());
    while end < body.len() && !body.is_char_boundary(end) {
        end += 1;
    }
    let slice = &body[start..end];
    let mut out = String::with_capacity(slice.len());
    let mut space = false;
    for ch in slice.chars() {
        if ch.is_whitespace() || ch.is_control() {
            if !space && !out.is_empty() {
                out.push(' ');
                space = true;
            }
        } else {
            space = false;
            out.push(ch);
        }
    }
    let out = out.trim().to_owned();
    if start > 0 {
        format!("…{out}")
    } else if end < body.len() {
        format!("{out}…")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::doc::{Doc, DocKind};
    use std::collections::BTreeMap;

    fn doc(title: &str, body: &str) -> Doc {
        Doc {
            title: title.into(),
            slug: "doc".into(),
            kind: DocKind::Note,
            tags: vec!["session".into()],
            created: "2026-11-16T00:00:00Z".into(),
            updated: "2026-11-16T00:00:00Z".into(),
            author: "user".into(),
            source: None,
            paths: Vec::new(),
            supersedes: Vec::new(),
            body: body.into(),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn parses_phrases_terms_and_exclusions() {
        let query = Query::parse(r#"restart "pane restart" -vim"#);
        assert_eq!(query.terms, vec!["restart"]);
        assert_eq!(query.phrases, vec!["pane restart"]);
        assert_eq!(query.excludes, vec!["vim"]);
    }

    #[test]
    fn requires_every_term_and_rejects_excluded_ones() {
        let doc = doc("Pane restart", "why panes restart after exit");
        assert!(score(&doc, &Query::parse("restart pane")).is_some());
        assert!(score(&doc, &Query::parse("restart missing")).is_none());
        assert!(score(&doc, &Query::parse("restart -pane")).is_none());
    }

    #[test]
    fn title_hits_outrank_body_hits() {
        let title_hit = score(
            &doc("restart policy", "unrelated"),
            &Query::parse("restart"),
        );
        let body_hit = score(&doc("policy", "restart happens"), &Query::parse("restart"));
        assert!(title_hit.unwrap().0 > body_hit.unwrap().0);
    }

    #[test]
    fn snippets_are_single_line_and_bounded() {
        let doc = doc("x", &format!("before {} after", "y".repeat(400)));
        let (_, snippet) = score(&doc, &Query::parse("before")).unwrap();
        assert!(!snippet.contains('\n'));
        assert!(snippet.len() <= 200, "{snippet:?}");
    }

    #[test]
    fn empty_query_still_scores() {
        let doc = doc("anything", "body");
        assert!(score(&doc, &Query::parse("")).is_some());
    }
}
