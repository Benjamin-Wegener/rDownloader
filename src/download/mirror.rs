//! Mirror selection: a file may be available from several URLs.
//!
//! Strategy (similar to aria2's "feedback" selector):
//! 1. Mirrors without any speed sample yet are tried first, spread over the parts
//!    so that every mirror gets some load and a first measurement.
//! 2. After that the mirror with the best measured speed wins.
//! 3. A mirror that fails three times in a row, or serves a different file, is
//!    dropped (unless nothing else is left).

use std::sync::Mutex;
use url::Url;

const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// Weight of the newest sample in the speed average.
const NEW_SAMPLE_WEIGHT: f64 = 0.5;

/// A mirror served bytes that do not belong to the file (different size).
#[derive(Debug)]
pub struct MirrorMismatch;

impl std::fmt::Display for MirrorMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("mirror serves a different file")
    }
}

impl std::error::Error for MirrorMismatch {}

#[derive(Clone, Default)]
struct Stat {
    speed: Option<f64>,
    failures: u32,
    banned: bool,
}

pub struct MirrorSelector {
    urls: Vec<Url>,
    stats: Mutex<Vec<Stat>>,
}

impl MirrorSelector {
    /// `urls[0]` is the primary URL; the rest are alternatives in priority order.
    /// Duplicates are removed.
    pub fn new(urls: Vec<Url>) -> Self {
        let mut unique: Vec<Url> = Vec::new();
        for u in urls {
            if !unique.contains(&u) {
                unique.push(u);
            }
        }
        let n = unique.len();
        Self {
            urls: unique,
            stats: Mutex::new(vec![Stat::default(); n]),
        }
    }

    pub fn len(&self) -> usize {
        self.urls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.urls.is_empty()
    }

    pub fn url(&self, index: usize) -> &Url {
        &self.urls[index]
    }

    /// Picks a mirror for `part`. `avoid` is the mirror that just failed for this part.
    pub fn pick(&self, part: usize, avoid: Option<usize>) -> usize {
        let stats = self.stats.lock().unwrap();
        let mut usable: Vec<usize> = (0..self.urls.len()).filter(|&i| !stats[i].banned).collect();
        if usable.is_empty() {
            usable = (0..self.urls.len()).collect();
        }
        if usable.len() > 1 {
            if let Some(a) = avoid {
                usable.retain(|&i| i != a);
            }
        }
        let untried: Vec<usize> = usable
            .iter()
            .copied()
            .filter(|&i| stats[i].speed.is_none())
            .collect();
        if !untried.is_empty() {
            return untried[part % untried.len()];
        }
        usable
            .into_iter()
            .max_by(|&a, &b| {
                let (sa, sb) = (stats[a].speed.unwrap_or(0.0), stats[b].speed.unwrap_or(0.0));
                sa.total_cmp(&sb).then(b.cmp(&a)) // ties: lower index = higher priority
            })
            .expect("at least one mirror")
    }

    pub fn record_success(&self, index: usize, bytes: u64, secs: f64) {
        if secs <= 0.0 {
            return;
        }
        let mut stats = self.stats.lock().unwrap();
        let s = &mut stats[index];
        let sample = bytes as f64 / secs;
        s.speed = Some(s.speed.map_or(sample, |old| {
            (1.0 - NEW_SAMPLE_WEIGHT) * old + NEW_SAMPLE_WEIGHT * sample
        }));
        s.failures = 0;
    }

    pub fn record_failure(&self, index: usize) {
        let mut stats = self.stats.lock().unwrap();
        let s = &mut stats[index];
        s.failures += 1;
        if s.failures >= MAX_CONSECUTIVE_FAILURES {
            s.banned = true;
        }
    }

    /// Drops a mirror for good (it serves another file).
    pub fn ban(&self, index: usize) {
        self.stats.lock().unwrap()[index].banned = true;
    }

    pub fn is_banned(&self, index: usize) -> bool {
        self.stats.lock().unwrap()[index].banned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(n: usize) -> MirrorSelector {
        MirrorSelector::new(
            (0..n)
                .map(|i| Url::parse(&format!("http://m{i}.test/f")).unwrap())
                .collect(),
        )
    }

    #[test]
    fn duplicates_are_removed() {
        let u = Url::parse("http://a.test/f").unwrap();
        assert_eq!(MirrorSelector::new(vec![u.clone(), u]).len(), 1);
    }

    #[test]
    fn parts_are_spread_over_untried_mirrors() {
        let s = sel(3);
        let picks: Vec<_> = (0..6).map(|p| s.pick(p, None)).collect();
        assert_eq!(picks, [0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn fastest_measured_mirror_wins() {
        let s = sel(3);
        s.record_success(0, 1_000, 1.0);
        s.record_success(1, 9_000, 1.0);
        s.record_success(2, 4_000, 1.0);
        assert_eq!(s.pick(0, None), 1);
        assert_eq!(s.pick(7, None), 1);
    }

    #[test]
    fn untried_mirrors_get_a_chance_before_measured_ones_win() {
        let s = sel(2);
        s.record_success(0, 1_000, 1.0);
        assert_eq!(s.pick(0, None), 1);
    }

    #[test]
    fn failing_mirror_is_avoided_then_banned() {
        let s = sel(2);
        assert_eq!(s.pick(0, Some(0)), 1, "avoid the mirror that just failed");
        for _ in 0..3 {
            s.record_failure(0);
        }
        assert!(s.is_banned(0));
        for p in 0..4 {
            assert_eq!(s.pick(p, None), 1);
        }
    }

    #[test]
    fn success_resets_the_failure_count() {
        let s = sel(2);
        s.record_failure(0);
        s.record_failure(0);
        s.record_success(0, 100, 1.0);
        s.record_failure(0);
        assert!(!s.is_banned(0));
    }

    #[test]
    fn with_everything_banned_a_mirror_is_still_returned() {
        let s = sel(2);
        s.ban(0);
        s.ban(1);
        assert!(s.pick(0, None) < 2);
    }

    #[test]
    fn single_mirror_is_always_used_even_if_avoided() {
        let s = sel(1);
        assert_eq!(s.pick(3, Some(0)), 0);
    }
}

/// Side table that carries mirror and piece information from an import (e.g. a
/// Metalink) to the job that is created later from the primary URL alone.
pub mod hints {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use url::Url;

    use crate::download::pieces::PieceHashes;

    /// Upper bound so a huge import cannot grow the table without limit.
    const MAX_ENTRIES: usize = 20_000;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Hint {
        pub mirrors: Vec<Url>,
        pub pieces: Option<PieceHashes>,
    }

    fn table() -> &'static Mutex<HashMap<String, Hint>> {
        static TABLE: OnceLock<Mutex<HashMap<String, Hint>>> = OnceLock::new();
        TABLE.get_or_init(Default::default)
    }

    pub fn register(primary: &Url, mirrors: Vec<Url>, pieces: Option<PieceHashes>) {
        if mirrors.is_empty() && pieces.is_none() {
            return;
        }
        let mut t = table().lock().unwrap();
        if t.len() >= MAX_ENTRIES && !t.contains_key(primary.as_str()) {
            return;
        }
        t.insert(primary.to_string(), Hint { mirrors, pieces });
    }

    pub fn lookup(primary: &Url) -> Option<Hint> {
        table().lock().unwrap().get(primary.as_str()).cloned()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn register_and_lookup() {
            let p = Url::parse("http://hints.test/unique-1").unwrap();
            let m = Url::parse("http://mirror.test/unique-1").unwrap();
            assert!(lookup(&p).is_none());
            register(&p, vec![m.clone()], None);
            assert_eq!(lookup(&p).unwrap().mirrors, vec![m]);
        }

        #[test]
        fn empty_hints_are_not_stored() {
            let p = Url::parse("http://hints.test/unique-2").unwrap();
            register(&p, vec![], None);
            assert!(lookup(&p).is_none());
        }
    }
}
