//! Time windows: change the speed limit or pause downloads at certain times.
//!
//! ```toml
//! [[schedule]]
//! days = ["mon", "tue", "wed", "thu", "fri"]   # empty = every day
//! from = "08:00"
//! to = "18:00"          # may be earlier than `from`: the window then crosses midnight
//! speed_kib = 200       # limit inside the window (0 = unlimited)
//!
//! [[schedule]]
//! from = "01:00"
//! to = "05:00"
//! pause = true          # downloads wait inside this window
//! ```
//! The first matching rule wins. Outside every window the configured
//! `max_speed_kib` applies. Times are local time.

use chrono::{Datelike, NaiveTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    pub days: Vec<String>,
    pub from: String,
    pub to: String,
    pub speed_kib: u64,
    pub pause: bool,
}

/// What applies at a given moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Limit in KiB/s (0 = unlimited).
    Limit(u64),
    Pause,
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

fn parse_day(s: &str) -> Option<Weekday> {
    s.trim().to_ascii_lowercase().parse::<Weekday>().ok()
}

impl Rule {
    /// Problems that make the rule unusable (shown once at start-up).
    pub fn validate(&self) -> Result<(), String> {
        let (from, to) = (parse_time(&self.from), parse_time(&self.to));
        if from.is_none() || to.is_none() {
            return Err(format!(
                "schedule: times must look like HH:MM (from={:?}, to={:?})",
                self.from, self.to
            ));
        }
        if from == to {
            return Err("schedule: `from` and `to` must differ".into());
        }
        if let Some(bad) = self.days.iter().find(|d| parse_day(d).is_none()) {
            return Err(format!("schedule: unknown day {bad:?} (use mon, tue, ...)"));
        }
        Ok(())
    }

    /// Whether the rule is active on `weekday` at `time`. A window that crosses
    /// midnight belongs to the day it *starts* on.
    pub fn active(&self, weekday: Weekday, time: NaiveTime) -> bool {
        let (Some(from), Some(to)) = (parse_time(&self.from), parse_time(&self.to)) else {
            return false;
        };
        if from == to {
            return false;
        }
        let on = |d: Weekday| {
            self.days.is_empty()
                || self
                    .days
                    .iter()
                    .filter_map(|x| parse_day(x))
                    .any(|x| x == d)
        };
        if from < to {
            on(weekday) && time >= from && time < to
        } else {
            (on(weekday) && time >= from) || (on(weekday.pred()) && time < to)
        }
    }

    fn effect(&self) -> Effect {
        if self.pause {
            Effect::Pause
        } else {
            Effect::Limit(self.speed_kib)
        }
    }
}

/// The effect of the first rule that is active now, if any. Invalid rules are skipped.
pub fn effect_at(rules: &[Rule], weekday: Weekday, time: NaiveTime) -> Option<Effect> {
    rules
        .iter()
        .filter(|r| r.validate().is_ok())
        .find(|r| r.active(weekday, time))
        .map(Rule::effect)
}

/// How often the limiter is updated from the timetable.
const TICK: Duration = Duration::from_secs(20);

/// Keeps the global limiter in line with the timetable. Does nothing without rules.
pub fn spawn(rules: Vec<Rule>, default_speed_kib: u64) {
    for rule in &rules {
        if let Err(e) = rule.validate() {
            tracing::warn!("{e}; rule ignored");
        }
    }
    if rules.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let limiter = super::limiter::global();
        loop {
            let now = chrono::Local::now();
            let time = NaiveTime::from_hms_opt(now.hour(), now.minute(), 0).unwrap_or_default();
            match effect_at(&rules, now.weekday(), time) {
                Some(Effect::Pause) => limiter.set_paused(true),
                Some(Effect::Limit(kib)) => {
                    limiter.set_paused(false);
                    limiter.set_rate(kib.saturating_mul(1024));
                }
                None => {
                    limiter.set_paused(false);
                    limiter.set_rate(default_speed_kib.saturating_mul(1024));
                }
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn rule(days: &[&str], from: &str, to: &str, speed: u64, pause: bool) -> Rule {
        Rule {
            days: days.iter().map(|d| d.to_string()).collect(),
            from: from.into(),
            to: to.into(),
            speed_kib: speed,
            pause,
        }
    }

    #[test]
    fn simple_window() {
        let r = rule(&[], "08:00", "18:00", 100, false);
        assert!(r.active(Weekday::Mon, t(8, 0)));
        assert!(r.active(Weekday::Mon, t(17, 59)));
        assert!(!r.active(Weekday::Mon, t(18, 0)), "end is exclusive");
        assert!(!r.active(Weekday::Mon, t(7, 59)));
    }

    #[test]
    fn window_across_midnight_belongs_to_its_start_day() {
        let r = rule(&["fri"], "22:00", "06:00", 0, true);
        assert!(r.active(Weekday::Fri, t(23, 0)));
        assert!(
            r.active(Weekday::Sat, t(5, 0)),
            "the night continues on Saturday morning"
        );
        assert!(
            !r.active(Weekday::Fri, t(5, 0)),
            "Friday morning belongs to Thursday night"
        );
        assert!(!r.active(Weekday::Sat, t(23, 0)));
    }

    #[test]
    fn day_selection() {
        let r = rule(&["Sat", "sun"], "00:00", "23:59", 1, false);
        assert!(r.active(Weekday::Sun, t(12, 0)));
        assert!(!r.active(Weekday::Mon, t(12, 0)));
    }

    #[test]
    fn first_matching_rule_wins() {
        let rules = vec![
            rule(&[], "01:00", "05:00", 0, true),
            rule(&[], "00:00", "12:00", 300, false),
        ];
        assert_eq!(
            effect_at(&rules, Weekday::Tue, t(2, 0)),
            Some(Effect::Pause)
        );
        assert_eq!(
            effect_at(&rules, Weekday::Tue, t(9, 0)),
            Some(Effect::Limit(300))
        );
        assert_eq!(effect_at(&rules, Weekday::Tue, t(15, 0)), None);
        assert_eq!(effect_at(&[], Weekday::Tue, t(2, 0)), None);
    }

    #[test]
    fn invalid_rules_are_reported_and_skipped() {
        for bad in [
            rule(&[], "25:00", "06:00", 0, false),
            rule(&[], "08:00", "08:00", 0, false),
            rule(&["funday"], "01:00", "02:00", 0, false),
            Rule::default(),
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
            assert!(!bad.active(Weekday::Mon, t(8, 0)));
        }
        let rules = vec![
            rule(&[], "xx", "yy", 5, false),
            rule(&[], "00:00", "23:59", 7, false),
        ];
        assert_eq!(
            effect_at(&rules, Weekday::Mon, t(8, 0)),
            Some(Effect::Limit(7))
        );
    }

    #[test]
    fn config_parses_from_toml() {
        #[derive(Deserialize)]
        struct W {
            schedule: Vec<Rule>,
        }
        let w: W = toml::from_str("[[schedule]]\nfrom = \"08:00\"\nto = \"18:00\"\nspeed_kib = 50\n[[schedule]]\nfrom=\"01:00\"\nto=\"05:00\"\npause=true").unwrap();
        assert_eq!(w.schedule.len(), 2);
        assert!(w.schedule[0].validate().is_ok() && w.schedule[1].pause);
    }
}
