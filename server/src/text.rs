//! Text preparation shared by indexing and querying, so both sides tokenize identically.

use std::collections::HashMap;
use tantivy::tokenizer::{AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer};

pub const TOKENIZER: &str = "geo";
pub const WS_TOKENIZER: &str = "ws";
const MAX_PREFIX_CHARS: usize = 20;

/// Space-separated leading letters (1..=20 chars) of every word: "via roma" ->
/// "v vi via r ro rom roma". Indexed so a half-typed word is a single term lookup.
pub fn prefixes(words: &[String]) -> String {
    let mut out = String::new();
    let mut seen = std::collections::HashSet::new();
    for w in words {
        let mut end = 0;
        for c in w.chars().take(MAX_PREFIX_CHARS) {
            end += c.len_utf8();
            let p = &w[..end];
            if seen.insert(p.to_string()) {
                out.push_str(p);
                out.push(' ');
            }
        }
    }
    out
}

/// The searchable key of a whole name: its tokens joined by single spaces.
pub fn exact_key(words: &[String]) -> String {
    words.join(" ")
}

pub fn analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(48))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0x20000..=0x2FFFF)
}

/// Drops dots ("p.za" -> "pza", "St. John" -> "St John") and splits CJK runs (which have no
/// spaces) into overlapping character pairs, so "千代田" matches "東京都千代田区".
pub fn prep(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let chars: Vec<char> = s.chars().filter(|&c| c != '.').collect();
    let mut i = 0;
    while i < chars.len() {
        if is_cjk(chars[i]) {
            let start = i;
            while i < chars.len() && is_cjk(chars[i]) {
                i += 1;
            }
            out.push(' ');
            if i - start == 1 {
                out.push(chars[start]);
            }
            for w in chars[start..i].windows(2) {
                out.push(w[0]);
                out.push(w[1]);
                out.push(' ');
            }
            out.push(' ');
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

pub fn tokens(a: &mut TextAnalyzer, s: &str) -> Vec<String> {
    let prepared = prep(s);
    let mut stream = a.token_stream(&prepared);
    let mut out: Vec<String> = Vec::new();
    while stream.advance() {
        let t = stream.token().text.clone();
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Ref values plus a separator-free variant ("I-95" also indexed as "i95").
pub fn ref_text(refs: &str) -> String {
    let mut s = String::new();
    for r in refs.split(';') {
        s.push_str(r);
        s.push(' ');
        let compact: String = r.chars().filter(|c| c.is_alphanumeric()).collect();
        if compact.len() != r.len() {
            s.push_str(&compact);
            s.push(' ');
        }
    }
    s
}

/// Query-time abbreviation table (see `abbrev.tsv`).
pub struct Abbrev {
    exact: HashMap<String, Vec<String>>,
    suffix: Vec<(String, Vec<String>)>,
}

impl Abbrev {
    pub fn parse(text: &str) -> Self {
        let mut a = Abbrev { exact: HashMap::new(), suffix: Vec::new() };
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let Some((k, v)) = line.split_once('\t') else { continue };
            let exps = v.split('|').map(|e| e.trim().trim_start_matches('-').to_string());
            if let Some(suf) = k.strip_prefix('-') {
                a.suffix.push((suf.to_string(), exps.collect()));
            } else {
                let e = a.exact.entry(k.to_string()).or_default();
                for x in exps {
                    if !e.contains(&x) {
                        e.push(x);
                    }
                }
            }
        }
        a
    }

    /// The token itself plus its expansions. Multi-word expansions ("strada statale") are skipped
    /// here; their single-word parts are matched by the other query words anyway.
    pub fn expand(&self, tok: &str) -> Vec<String> {
        let mut out = vec![tok.to_string()];
        for e in self.exact.get(tok).into_iter().flatten() {
            if !e.contains(' ') && !out.contains(e) {
                out.push(e.clone());
            }
        }
        for (suf, exps) in &self.suffix {
            if let Some(stem) = tok.strip_suffix(suf.as_str())
                && stem.chars().count() >= 3
            {
                for e in exps {
                    let w = format!("{stem}{e}");
                    if !out.contains(&w) {
                        out.push(w);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prep_and_tokens() {
        let mut a = analyzer();
        assert_eq!(tokens(&mut a, "P.za Garibaldi, 12"), ["pza", "garibaldi", "12"]);
        assert_eq!(tokens(&mut a, "Hauptstraße"), ["hauptstrasse"]);
        assert_eq!(tokens(&mut a, "Rue de l'Église"), ["rue", "de", "l", "eglise"]);
        assert_eq!(tokens(&mut a, "東京都千代田"), ["東京", "京都", "都千", "千代", "代田"]);
        assert_eq!(ref_text("I-95;A1"), "I-95 I95 A1 ");
        let w = tokens(&mut a, "Via Roma");
        assert_eq!(prefixes(&w), "v vi via r ro rom roma ");
        assert_eq!(exact_key(&w), "via roma");
    }

    #[test]
    fn abbreviations() {
        let a = Abbrev::parse(include_str!("../abbrev.tsv"));
        assert!(a.expand("st").contains(&"street".to_string()));
        assert!(a.expand("hauptstr").contains(&"hauptstrasse".to_string()));
        let pza = a.expand("pza");
        assert!(pza.contains(&"piazza".to_string()) && pza.contains(&"plaza".to_string()));
        assert_eq!(a.expand("roma"), ["roma"]);
        assert!(!a.expand("ss").iter().any(|e| e.contains(' ')));
    }
}
