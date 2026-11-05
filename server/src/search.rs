//! Query pipeline: tokenize -> drop address noise / fix typos / expand abbreviations ->
//! AND match (order-free) -> ring search outward from the point -> relax if needed.

use crate::{Index, SearchError, text};
use anyhow::Result;
use geobox_core::{CELL_LEVELS, cell_term, cell_xy, haversine_m};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tantivy::collector::TopDocs;
use tantivy::columnar::ColumnValues;
use tantivy::index::SegmentId;
use tantivy::query::{
    BooleanQuery, BoostQuery, ConstScoreQuery, FuzzyTermQuery, Occur, Query as TQuery, TermQuery, TermSetQuery,
};
use tantivy::schema::{Field, IndexRecordOption, Value};
use tantivy::{DocAddress, Searcher, SegmentReader, TantivyDocument, Term};

#[derive(Debug, Clone, Default)]
pub struct Query {
    pub q: String,
    /// Optional point (degrees) to rank by distance.
    pub point: Option<(f64, f64)>,
    pub limit: usize,
    /// Preferred language for `display_name` (`name:<lang>`).
    pub lang: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub name: String,
    pub display_name: String,
    #[serde(rename = "ref", skip_serializing_if = "String::is_empty")]
    pub refs: String,
    pub class: u64,
    pub place: String,
    pub country_code: String,
    pub lat: f64,
    pub lon: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_m: Option<f64>,
    pub score: f32,
    #[serde(skip)]
    alt: String,
}

impl Hit {
    pub(crate) fn approx_size(&self) -> usize {
        std::mem::size_of::<Hit>() + self.name.len() + self.refs.len() + self.place.len() + self.alt.len() + 8
    }
}

/// Field weights within the text score.
const NAME_FIELDS: [(usize, f32); 3] = [(0, 3.0), (1, 1.5), (2, 2.5)]; // name, alt, ref
const PLACE_BOOST: f32 = 1.0;
/// A half-typed word matching the start of a street-name word (same score for every match).
const PREFIX_SCORE: f32 = 2.0;
/// Bonus when the query is exactly one of the street's names.
const EXACT_SCORE: f32 = 4.0;
/// Most frequent place-name completions of a half-typed word ("via roma mil" -> milano).
const PLACE_COMPLETIONS: usize = 8;
const PLACE_COMPLETION_SCORE: f32 = 0.5;
/// Look-alike words tried for a possibly misspelled word, and their score per field boost.
const FUZZY_TERMS: usize = 32;
const FUZZY_SCORE: f32 = 1.0;
const FUZZY_MAX_DF: u64 = 200;
/// With only very common words, text scores barely differ: skip the extra (wider) ring.
const EXTRA_RING_MAX_DF: u64 = 50_000;
/// Leaving out a word is only allowed if a fairly specific word remains: "via di" (what is left of
/// "via di casal boccone") matches streets everywhere and would return confident junk.
const DROP_ONE_MAX_DF: u64 = 50_000;
/// Below this many possible matches, one worldwide query is cheaper than ring search.
const RING_MIN_MATCHES: u64 = 3_000;
/// Ring search: 3x3 cells at these levels around the point, then the whole world.
const RINGS: [Option<u8>; 5] = [Some(12), Some(10), Some(8), Some(6), None];
const MAX_WORDS: usize = 8;

/// One query word and the index terms it may match.
#[derive(Debug, Clone)]
struct Group {
    word: String,
    /// The word itself and its abbreviation expansions that exist in the index.
    terms: Vec<String>,
    /// Half-typed last word: matches street-name words starting with it.
    prefix: bool,
    place_completions: Vec<String>,
    /// Indexed words within 1-2 typos of `word` (expanded once per query).
    fuzzy_terms: Vec<String>,
    fuzzied: bool,
    /// Upper bound of streets this word can match (sum of doc frequencies).
    df: u64,
    /// A bare number (house number, km marker): adds score when it matches, never required.
    optional: bool,
    /// Terms in so many streets that scoring them costs a lot and ranks nothing
    /// (`PREFIX_MARK` = the prefix); they still have to match.
    dense: Vec<String>,
}

thread_local! {
    /// (ring zoom or None = worldwide, ms, hits) of each index search in the current query,
    /// for the slow-query log.
    static SEARCH_LOG: std::cell::RefCell<Vec<(Option<u8>, f32, usize)>> = const { std::cell::RefCell::new(Vec::new()) };
}

const PREFIX_MARK: &str = "\u{0}prefix";
/// Words in more streets than this get no relevance scoring (their idf is near zero anyway).
const SCORE_MAX_DF: u64 = 200_000;

fn is_number(w: &str) -> bool {
    !w.is_empty() && w.chars().all(|c| c.is_ascii_digit())
}

impl Index {
    /// Async search with load shedding: at most `search_threads` run at once and at most
    /// `search_threads * queue_factor` wait; beyond that it fails fast with `Overloaded`.
    pub async fn search(self: &Arc<Self>, q: Query) -> Result<Vec<Hit>, SearchError> {
        let limit = self.cfg.search_threads * (self.cfg.queue_factor + 1);
        if self.queued.fetch_add(1, Ordering::AcqRel) >= limit {
            self.queued.fetch_sub(1, Ordering::AcqRel);
            return Err(SearchError::Overloaded);
        }
        let res = async {
            let _permit = self.permits.clone().acquire_owned().await.map_err(anyhow::Error::from)?;
            let me = self.clone();
            Ok(tokio::task::spawn_blocking(move || me.search_blocking(&q)).await.map_err(anyhow::Error::from)??)
        }
        .await;
        self.queued.fetch_sub(1, Ordering::AcqRel);
        res
    }

    /// The same search, synchronously (for callers with their own thread pool).
    pub fn search_blocking(&self, q: &Query) -> Result<Vec<Hit>> {
        let limit = q.limit.clamp(1, 100);
        // ranking uses the point snapped to ~1 km so nearby users share cache entries
        let snapped = q.point.map(|(la, lo)| ((la * 100.0).round() / 100.0, (lo * 100.0).round() / 100.0));
        let ends_open = !q.q.ends_with(|c: char| !c.is_alphanumeric());
        let key = format!(
            "{}|{}|{:?}|{limit}|{}|{ends_open}",
            self.generation.load(Ordering::Relaxed),
            q.q.trim().to_lowercase(),
            snapped,
            q.lang.as_deref().unwrap_or("")
        );
        let hits = match self.cache.get(&key) {
            Some(h) => {
                metrics::counter!("geobox_cache_hits_total").increment(1);
                h
            }
            None => {
                let h = Arc::new(self.run(q, snapped, limit, ends_open)?);
                self.cache.insert(key, h.clone());
                h
            }
        };
        Ok(hits
            .iter()
            .map(|h| {
                let mut h = h.clone();
                h.distance_m = q.point.map(|(la, lo)| {
                    haversine_m((la * 1e7) as i32, (lo * 1e7) as i32, (h.lat * 1e7) as i32, (h.lon * 1e7) as i32)
                        .round()
                });
                h
            })
            .collect())
    }

    fn fields(&self) -> [Field; 4] {
        [self.f.name, self.f.alt, self.f.refs, self.f.place]
    }

    fn run(&self, q: &Query, point: Option<(f64, f64)>, want: usize, ends_open: bool) -> Result<Vec<Hit>> {
        // a few extra, so the list is still full after merging duplicate pieces of one street
        let limit = want + want / 2 + 2;
        let searcher = self.reader.searcher();
        let mut analyzer = text::analyzer();
        let started = std::time::Instant::now();
        SEARCH_LOG.with(|l| l.borrow_mut().clear());
        let mut words = text::tokens(&mut analyzer, &q.q);
        words.truncate(MAX_WORDS);
        // Exact-name bonus keys: the query without bare numbers ("via roma 12"), and every run of
        // 2+ words at its start or end, so "central park west new york" still rewards the street
        // named exactly "Central Park West" (and "berlin hauptstrasse" rewards "Hauptstraße").
        let no_numbers: Vec<String> = words.iter().filter(|w| !is_number(w)).cloned().collect();
        let mut exact = vec![text::exact_key(&words)];
        for n in (2..=no_numbers.len()).rev() {
            for key in [text::exact_key(&no_numbers[..n]), text::exact_key(&no_numbers[no_numbers.len() - n..])] {
                if !exact.contains(&key) {
                    exact.push(key);
                }
            }
        }
        let last = words.len().saturating_sub(1);
        let mut groups = Vec::new();
        for (i, w) in words.iter().enumerate() {
            if let Some(g) = self.plan_word(&searcher, w, i == last && ends_open)? {
                groups.push(g);
            }
        }
        if groups.is_empty() {
            return Ok(Vec::new());
        }
        if groups.iter().all(|g| g.optional) {
            groups.iter_mut().for_each(|g| g.optional = false); // a query of only numbers ("66")
        }
        let t_plan = started.elapsed();
        let point_e7 = point.map(|(la, lo)| ((la * 1e7) as i32, (lo * 1e7) as i32));
        // Tiers; the later ones only run when the earlier ones found nothing at all:
        // 1. every word as typed  2. every word, allowing typos  3. one word left out
        // (wrong city, extra words...). Earlier tiers always rank first.
        let mut scored = self.ring_search(&searcher, &groups, &exact, point_e7, limit)?;
        let t_tier1 = started.elapsed();
        let add_tier = |scored: &mut Vec<(f32, DocAddress)>, mut tier: Vec<(f32, DocAddress)>| {
            tier.retain(|(_, a)| !scored.iter().any(|(_, b)| a == b));
            tier.sort_by(|a, b| b.0.total_cmp(&a.0));
            scored.extend(tier);
            scored.truncate(limit);
        };
        let mut fuzzy = Vec::with_capacity(groups.len());
        let mut has_fuzzy = false;
        if scored.is_empty() {
            for g in &groups {
                let f = if g.optional { None } else { self.with_fuzzy(&searcher, g)? };
                has_fuzzy |= f.is_some();
                fuzzy.push(f.unwrap_or_else(|| g.clone()));
            }
        }
        if scored.is_empty() && has_fuzzy {
            let tier = self.ring_search(&searcher, &fuzzy, &exact, point_e7, limit)?;
            add_tier(&mut scored, tier);
        }
        let t_tier2 = started.elapsed();
        if scored.is_empty() && groups.iter().filter(|g| !g.optional).count() > 1 {
            let mut tier = Vec::new();
            for skip in (0..fuzzy.len()).filter(|&j| !fuzzy[j].optional) {
                let sub: Vec<Group> =
                    fuzzy.iter().enumerate().filter(|(j, _)| *j != skip).map(|(_, g)| g.clone()).collect();
                if sub.iter().filter(|g| !g.optional).map(|g| g.df).min().unwrap_or(u64::MAX) > DROP_ONE_MAX_DF {
                    continue;
                }
                for hit in self.ring_search(&searcher, &sub, &exact, point_e7, limit)? {
                    if !tier.iter().any(|(_, a)| *a == hit.1) {
                        tier.push(hit);
                    }
                }
            }
            add_tier(&mut scored, tier);
        }
        let t_tier3 = started.elapsed();
        let hits = scored
            .into_iter()
            .map(|(s, addr)| self.hit(&searcher, addr, s, q.lang.as_deref()))
            .collect::<Result<Vec<Hit>>>()
            .map(|h| dedupe(h, want));
        let total = started.elapsed();
        if total.as_millis() as u64 >= self.cfg.slow_query_ms {
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            tracing::warn!(
                q = %q.q,
                words = groups.len(),
                rarest = groups.iter().map(|g| g.df).min().unwrap_or(0),
                segments = searcher.segment_readers().len(),
                searches = SEARCH_LOG.with(|l| l.borrow().iter().map(|(r, ms, n)| format!("{}:{ms:.0}ms/{n}", r.map_or("all".to_string(), |z| format!("z{z}")))).collect::<Vec<_>>().join(" ")),
                "slow query: {:.0} ms (plan {:.0}, exact {:.0}, typos {:.0}, drop-one {:.0}, fetch {:.0})",
                ms(total), ms(t_plan), ms(t_tier1 - t_plan), ms(t_tier2 - t_tier1), ms(t_tier3 - t_tier2), ms(total - t_tier3)
            );
        }
        hits
    }

    /// Decides what a query word can match. `None` = ignore the word (house number,
    /// postcode, unknown noise).
    fn plan_word(&self, searcher: &Searcher, w: &str, prefix: bool) -> Result<Option<Group>> {
        let df = |f: Field, t: &str| searcher.doc_freq(&Term::from_field_text(f, t));
        let mut g = Group {
            word: w.to_string(),
            terms: Vec::new(),
            prefix: false,
            place_completions: Vec::new(),
            fuzzy_terms: Vec::new(),
            fuzzied: false,
            df: 0,
            optional: is_number(w),
            dense: Vec::new(),
        };
        for t in self.abbrev.expand(w) {
            let n = df(self.f.all, &t)?;
            if n > 0 {
                if n > SCORE_MAX_DF {
                    g.dense.push(t.clone());
                }
                g.terms.push(t);
                g.df += n;
            }
        }
        if prefix && !g.optional {
            let n = df(self.f.prefix, w)?;
            g.prefix = n > 0;
            if n > SCORE_MAX_DF {
                g.dense.push(PREFIX_MARK.to_string());
            }
            g.df += n;
            if w.chars().count() >= 3 {
                let (completions, n) = self.complete_place(searcher, w)?;
                g.place_completions = completions;
                g.df += n;
            }
        }
        if !g.terms.is_empty() || g.prefix || !g.place_completions.is_empty() {
            return Ok(Some(g));
        }
        // unknown word: a typo of a long-enough word, or noise (house number, postcode, "apt")
        self.with_fuzzy(searcher, &g)
    }

    /// The same group, also matching indexed words within 1 typo (2 from 11 letters).
    fn with_fuzzy(&self, searcher: &Searcher, g: &Group) -> Result<Option<Group>> {
        let len = g.word.chars().count();
        // A word found in hundreds of street names is real, not the misspelled one: expanding
        // it (north -> norte, nort...) only adds millions of matches.
        if g.fuzzied || len < 4 || g.df > FUZZY_MAX_DF || g.word.chars().any(|c| c.is_ascii_digit()) {
            return Ok(None);
        }
        let d = if len >= 11 { 2 } else { 1 };
        let automaton = FuzzyTermQuery::new(Term::from_field_text(self.f.name, &g.word), d, true).automaton()?;
        let mut counts: HashMap<Vec<u8>, u64> = HashMap::new();
        for seg in searcher.segment_readers() {
            // street-name words only: much smaller than `all` (place names in many languages),
            // and place-name typos are covered by prefix completion
            let inv = seg.inverted_index(self.f.names)?;
            let mut s = inv.terms().search(&automaton).into_stream()?;
            let mut scanned = 0;
            while s.advance() && scanned < 2_000 {
                if s.key() != g.word.as_bytes() {
                    *counts.entry(s.key().to_vec()).or_default() += s.value().doc_freq as u64;
                }
                scanned += 1;
            }
        }
        let mut v: Vec<(Vec<u8>, u64)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(FUZZY_TERMS);
        if v.is_empty() {
            return Ok(None);
        }
        let mut out = g.clone();
        out.fuzzied = true;
        out.df += v.iter().map(|x| x.1).sum::<u64>();
        out.dense
            .extend(v.iter().filter(|x| x.1 > SCORE_MAX_DF).filter_map(|(k, _)| String::from_utf8(k.clone()).ok()));
        out.fuzzy_terms = v.into_iter().filter_map(|(k, _)| String::from_utf8(k).ok()).collect();
        Ok(Some(out))
    }

    /// Most frequent place-name terms starting with `prefix`, plus their total doc frequency.
    fn complete_place(&self, searcher: &Searcher, prefix: &str) -> Result<(Vec<String>, u64)> {
        let mut upper = prefix.as_bytes().to_vec();
        upper.push(0xff);
        let mut counts: HashMap<Vec<u8>, u64> = HashMap::new();
        for seg in searcher.segment_readers() {
            let inv = seg.inverted_index(self.f.place)?;
            let mut s = inv.terms().range().ge(prefix.as_bytes()).lt(&upper).into_stream()?;
            let mut scanned = 0;
            while s.advance() && scanned < 5_000 {
                if s.key() != prefix.as_bytes() {
                    *counts.entry(s.key().to_vec()).or_default() += s.value().doc_freq as u64;
                }
                scanned += 1;
            }
        }
        let mut v: Vec<(Vec<u8>, u64)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(PLACE_COMPLETIONS);
        let total = v.iter().map(|x| x.1).sum();
        Ok((v.into_iter().filter_map(|(k, _)| String::from_utf8(k).ok()).collect(), total))
    }

    /// Matching only (no score): any of the word's terms in `field` (`all` or `names`), or the
    /// half-typed word as a name prefix. A single plain term when possible: unions are slow to
    /// skip through inside the AND of all words.
    fn match_query(&self, g: &Group, field: Field) -> Vec<Box<dyn TQuery>> {
        let basic = |f: Field, t: &str| -> Box<dyn TQuery> {
            Box::new(TermQuery::new(Term::from_field_text(f, t), IndexRecordOption::Basic))
        };
        // With a half-typed word, the prefix term already matches every name word starting with it
        // ("st" covers street, station...): those terms would only widen the OR.
        let covered = |t: &&String| g.prefix && t.len() > g.word.len() && t.starts_with(g.word.as_str());
        let mut q: Vec<Box<dyn TQuery>> =
            g.terms.iter().chain(&g.fuzzy_terms).filter(|t| !covered(t)).map(|t| basic(field, t)).collect();
        if g.prefix {
            q.push(basic(self.f.prefix, &g.word));
        }
        if field == self.f.all {
            q.extend(g.place_completions.iter().map(|t| basic(field, t)));
        }
        q
    }

    fn any_of(mut qs: Vec<Box<dyn TQuery>>) -> Box<dyn TQuery> {
        if qs.len() == 1 {
            return qs.pop().unwrap();
        }
        Box::new(BooleanQuery::new(qs.into_iter().map(|q| (Occur::Should, q)).collect()))
    }

    /// Scoring only: per-field relevance of the word (evaluated just for streets that matched).
    /// `None` when every term is dense: walking millions of postings per candidate would cost far
    /// more than the near-zero idf adds to the ranking.
    fn group_query(&self, g: &Group) -> Option<Box<dyn TQuery>> {
        let fields = self.fields();
        let mut targets: Vec<(Field, f32)> = NAME_FIELDS.iter().map(|&(i, b)| (fields[i], b)).collect();
        targets.push((self.f.place, PLACE_BOOST));
        let mut should: Vec<(Occur, Box<dyn TQuery>)> = Vec::new();
        let boosted = |q: Box<dyn TQuery>, b: f32| -> (Occur, Box<dyn TQuery>) {
            (Occur::Should, Box::new(BoostQuery::new(q, b)))
        };
        let sparse = |t: &&String| !g.dense.contains(t);
        for &(f, boost) in &targets {
            for t in g.terms.iter().filter(sparse) {
                should.push(boosted(
                    Box::new(TermQuery::new(Term::from_field_text(f, t), IndexRecordOption::WithFreqs)),
                    boost,
                ));
            }
            if f != self.f.refs {
                // fixed score: a rare look-alike word must not outscore a real match through idf
                for t in g.fuzzy_terms.iter().filter(sparse) {
                    let tq = TermQuery::new(Term::from_field_text(f, t), IndexRecordOption::Basic);
                    should.push((Occur::Should, Box::new(ConstScoreQuery::new(Box::new(tq), boost * FUZZY_SCORE))));
                }
            }
        }
        if g.prefix && !g.dense.iter().any(|d| d == PREFIX_MARK) {
            let tq = TermQuery::new(Term::from_field_text(self.f.prefix, &g.word), IndexRecordOption::Basic);
            should.push((Occur::Should, Box::new(ConstScoreQuery::new(Box::new(tq), PREFIX_SCORE))));
        }
        for c in &g.place_completions {
            let tq = TermQuery::new(Term::from_field_text(self.f.place, c), IndexRecordOption::Basic);
            should.push((Occur::Should, Box::new(ConstScoreQuery::new(Box::new(tq), PLACE_COMPLETION_SCORE))));
        }
        (!should.is_empty()).then(|| Box::new(BooleanQuery::new(should)) as Box<dyn TQuery>)
    }

    /// Every word must match somewhere; at least one must match the street's own names.
    fn build_query(&self, groups: &[Group], exact: &[String], ring: Option<Vec<Term>>) -> Box<dyn TQuery> {
        let zero = |q: Box<dyn TQuery>| -> Box<dyn TQuery> { Box::new(ConstScoreQuery::new(q, 0.0)) };
        let required: Vec<&Group> = groups.iter().filter(|g| !g.optional).collect();
        let mut clauses: Vec<(Occur, Box<dyn TQuery>)> =
            required.iter().map(|g| (Occur::Must, zero(Self::any_of(self.match_query(g, self.f.all))))).collect();
        let named: Vec<Box<dyn TQuery>> = required.iter().flat_map(|g| self.match_query(g, self.f.names)).collect();
        clauses.push((Occur::Must, zero(Self::any_of(named))));
        clauses.extend(groups.iter().filter_map(|g| self.group_query(g)).map(|q| (Occur::Should, q)));
        if let Some(cells) = ring {
            clauses.push((Occur::Must, Box::new(ConstScoreQuery::new(Box::new(TermSetQuery::new(cells)), 0.0))));
        }
        for key in exact {
            let eq = TermQuery::new(Term::from_field_text(self.f.exact, key), IndexRecordOption::Basic);
            clauses.push((Occur::Should, Box::new(ConstScoreQuery::new(Box::new(eq), EXACT_SCORE))));
        }
        Box::new(BooleanQuery::new(clauses))
    }

    fn ring_cells(&self, (lat, lon): (i32, i32), z: u8) -> Vec<Term> {
        let n = 1i64 << z;
        let (x, y) = cell_xy(lat, lon, z);
        let mut cells = Vec::with_capacity(9);
        for dy in -1..=1i64 {
            let yy = y as i64 + dy;
            if !(0..n).contains(&yy) {
                continue;
            }
            for dx in -1..=1i64 {
                let xx = (x as i64 + dx).rem_euclid(n); // wraps around the antimeridian
                cells.push(Term::from_field_u64(self.f.cell, cell_term(z, xx as u32, yy as u32)));
            }
        }
        cells
    }

    /// Searches growing rings around the point; once a ring yields `limit` results, one more
    /// (wider) ring is searched so a better text match slightly farther away still wins.
    fn ring_search(
        &self,
        searcher: &Searcher,
        groups: &[Group],
        exact: &[String],
        point: Option<(i32, i32)>,
        limit: usize,
    ) -> Result<Vec<(f32, DocAddress)>> {
        let w = self.cfg.weights;
        let cols = self.columns.read().clone();
        let collector = TopDocs::with_limit(limit).tweak_score(move |seg: &SegmentReader| {
            let SegColumns { lat, lon, class } =
                cols.get(&seg.segment_id()).cloned().unwrap_or_else(|| SegColumns::open(seg));
            move |doc, text_score| {
                let mut s = text_score * w.text - w.class * class.get_val(doc) as f32 / 9.0;
                if let Some((plat, plon)) = point {
                    s -= w.dist * (1.0 + approx_km(plat, plon, lat.get_val(doc), lon.get_val(doc))).ln();
                }
                s
            }
        });
        // Every word must match, so the rarest word bounds the result count. When it is small,
        // scoring all matches worldwide (distance included) in one pass beats several rings.
        let rarest = groups.iter().filter(|g| !g.optional).map(|g| g.df).min().unwrap_or(0);
        let few = rarest < RING_MIN_MATCHES;
        let timed = |ring: Option<u8>, q: &dyn TQuery| -> Result<Vec<(f32, DocAddress)>> {
            let t = std::time::Instant::now();
            let hits = searcher.search(q, &collector)?;
            SEARCH_LOG.with(|l| l.borrow_mut().push((ring, t.elapsed().as_secs_f32() * 1000.0, hits.len())));
            Ok(hits)
        };
        let Some(p) = point.filter(|_| !few) else {
            return timed(None, &*self.build_query(groups, exact, None));
        };
        debug_assert!(CELL_LEVELS.contains(&12) && CELL_LEVELS.contains(&6));
        let mut hits = Vec::new();
        let mut enough = false;
        for ring in RINGS {
            // Only very common words and still too few nearby: a worldwide pass would score
            // millions of streets for results thousands of km away. Return what was found.
            if ring.is_none() && rarest > EXTRA_RING_MAX_DF && !hits.is_empty() {
                break;
            }
            let q = self.build_query(groups, exact, ring.map(|z| self.ring_cells(p, z)));
            hits = timed(ring, &*q)?;
            if hits.len() >= limit {
                // the extra ring never goes worldwide: thousands of km of distance penalty
                // outweigh any better text match
                if enough || rarest > EXTRA_RING_MAX_DF || ring == Some(6) {
                    break;
                }
                enough = true;
            }
        }
        Ok(hits)
    }

    fn hit(&self, searcher: &Searcher, addr: DocAddress, score: f32, lang: Option<&str>) -> Result<Hit> {
        let d: TantivyDocument = searcher.doc(addr)?;
        let s = |f: Field| d.get_first(f).and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let i = |f: Field| d.get_first(f).and_then(|v| v.as_i64()).unwrap_or_default();
        let (name, refs, alt) = (s(self.f.d_name), s(self.f.d_ref), s(self.f.d_alt));
        let localized = lang.and_then(|l| {
            let key = format!("name:{l}=");
            alt.lines().find_map(|line| line.strip_prefix(key.as_str())).map(String::from)
        });
        let display_name = localized.unwrap_or_else(|| if name.is_empty() { refs.clone() } else { name.clone() });
        Ok(Hit {
            name,
            display_name,
            refs,
            class: d.get_first(self.f.class).and_then(|v| v.as_u64()).unwrap_or(9),
            place: s(self.f.d_place),
            country_code: s(self.f.d_cc),
            lat: i(self.f.lat) as f64 / 1e7,
            lon: i(self.f.lon) as f64 / 1e7,
            distance_m: None,
            score,
            alt,
        })
    }
}

/// One street can be stored as several pieces when its segments are far apart in OSM (more than
/// the generator's join distance). Same name + same place within this distance = one result.
const SAME_STREET_M: f64 = 1_500.0;

fn dedupe(hits: Vec<Hit>, want: usize) -> Vec<Hit> {
    let e7 = |v: f64| (v * 1e7).round() as i32;
    let mut out: Vec<Hit> = Vec::with_capacity(want);
    for h in hits {
        let dup = out.iter().any(|k| {
            k.name == h.name
                && k.refs == h.refs
                && k.place == h.place
                && haversine_m(e7(k.lat), e7(k.lon), e7(h.lat), e7(h.lon)) < SAME_STREET_M
        });
        if !dup {
            out.push(h);
            if out.len() == want {
                break;
            }
        }
    }
    out
}

/// Fast-field columns of one segment. Opening a column costs ~4 ms on a planet index, far more
/// than a whole search, so they are opened once per segment and cached (see `Index::columns`).
#[derive(Clone)]
pub(crate) struct SegColumns {
    lat: Arc<dyn ColumnValues<i64>>,
    lon: Arc<dyn ColumnValues<i64>>,
    class: Arc<dyn ColumnValues<u64>>,
}

impl SegColumns {
    fn open(seg: &SegmentReader) -> Self {
        let ff = seg.fast_fields();
        SegColumns {
            lat: ff.i64("lat").expect("lat fast field").first_or_default_col(0),
            lon: ff.i64("lon").expect("lon fast field").first_or_default_col(0),
            class: ff.u64("class").expect("class fast field").first_or_default_col(9),
        }
    }
}

pub(crate) type ColumnCache = Arc<HashMap<SegmentId, SegColumns>>;

/// Columns for every segment of the current searcher; call after each reader reload.
pub(crate) fn load_columns(searcher: &Searcher) -> ColumnCache {
    Arc::new(searcher.segment_readers().iter().map(|s| (s.segment_id(), SegColumns::open(s))).collect())
}

/// Fast flat-earth distance for ranking (exact haversine is used for the reported distance).
fn approx_km(lat1: i32, lon1: i32, lat2: i64, lon2: i64) -> f32 {
    let dlat = (lat2 - lat1 as i64) as f32 * 1e-7;
    let mut dlon = ((lon2 - lon1 as i64) as f32 * 1e-7).abs();
    if dlon > 180.0 {
        dlon = 360.0 - dlon;
    }
    let x = dlon * ((lat1 as f32 * 1e-7).to_radians()).cos();
    111.195 * (dlat * dlat + x * x).sqrt()
}
