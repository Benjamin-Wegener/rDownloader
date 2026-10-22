//! View logic of the window without any drawing: which downloads are shown and the
//! totals of the overview panel. Pure functions, unit-tested.

use uuid::Uuid;

use crate::download::{JobProgress, JobStatus, PartStatus};

/// Which downloads the filter menu lets through.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum StateFilter {
    #[default]
    All,
    Running,
    Paused,
    Finished,
    Failed,
}

/// What the search box looks at.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SearchScope {
    #[default]
    Name,
    Host,
    Url,
}

pub fn is_running(status: &JobStatus) -> bool {
    matches!(
        status,
        JobStatus::Resolving | JobStatus::Downloading | JobStatus::Merging
    )
}

pub fn state_matches(filter: StateFilter, status: &JobStatus) -> bool {
    match filter {
        StateFilter::All => true,
        StateFilter::Running => is_running(status) || matches!(status, JobStatus::Queued),
        StateFilter::Paused => matches!(status, JobStatus::Paused),
        StateFilter::Finished => matches!(status, JobStatus::Done),
        StateFilter::Failed => matches!(status, JobStatus::Failed(_) | JobStatus::Cancelled),
    }
}

/// Host name of a link, or an empty string if it has none.
pub fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

/// Whether `job` passes the state filter and the search text (case-insensitive,
/// an empty search matches everything).
pub fn matches(job: &JobProgress, filter: StateFilter, scope: SearchScope, query: &str) -> bool {
    if !state_matches(filter, &job.status) {
        return false;
    }
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let haystack = match scope {
        SearchScope::Name => job.filename.to_lowercase(),
        SearchScope::Host => host_of(&job.url).to_lowercase(),
        SearchScope::Url => job.url.to_lowercase(),
    };
    haystack.contains(&query)
}

/// Totals shown in the overview panel (over all downloads, not only the visible ones).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub links: usize,
    pub running: usize,
    pub finished: usize,
    pub total_bytes: u64,
    pub loaded_bytes: u64,
    pub remaining_bytes: u64,
    pub bytes_per_sec: u64,
    pub eta_secs: Option<u64>,
    pub connections: usize,
}

impl Totals {
    pub fn compute(jobs: &[JobProgress], speed_of: impl Fn(Uuid) -> u64) -> Self {
        let mut t = Totals {
            links: jobs.len(),
            ..Default::default()
        };
        for job in jobs {
            if matches!(job.status, JobStatus::Done) {
                t.finished += 1;
            }
            t.total_bytes += job.total_bytes;
            t.loaded_bytes += job.bytes_done.min(job.total_bytes.max(job.bytes_done));
            if is_running(&job.status) {
                t.running += 1;
            }
            if matches!(job.status, JobStatus::Downloading) {
                t.bytes_per_sec += speed_of(job.id);
                // Without part information one connection is open.
                let open = job
                    .parts
                    .iter()
                    .filter(|p| {
                        matches!(
                            p.status,
                            PartStatus::Downloading | PartStatus::Retrying { .. }
                        )
                    })
                    .count();
                t.connections += open.max(1);
            }
            // Unfinished downloads of unknown size cannot be counted as remaining.
            if !matches!(
                job.status,
                JobStatus::Done | JobStatus::Failed(_) | JobStatus::Cancelled
            ) {
                t.remaining_bytes += job.total_bytes.saturating_sub(job.bytes_done);
            }
        }
        t.eta_secs = (t.bytes_per_sec > 0).then(|| t.remaining_bytes.div_ceil(t.bytes_per_sec));
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::PartProgress;

    fn job(
        n: u128,
        name: &str,
        url: &str,
        status: JobStatus,
        done: u64,
        total: u64,
    ) -> JobProgress {
        JobProgress {
            id: Uuid::from_u128(n),
            url: url.into(),
            filename: name.into(),
            total_bytes: total,
            bytes_done: done,
            parts: vec![],
            status,
        }
    }

    #[test]
    fn state_filters() {
        use JobStatus::*;
        assert!(state_matches(StateFilter::All, &Done));
        assert!(state_matches(StateFilter::Running, &Downloading));
        assert!(state_matches(StateFilter::Running, &Queued));
        assert!(!state_matches(StateFilter::Running, &Paused));
        assert!(state_matches(StateFilter::Paused, &Paused));
        assert!(state_matches(StateFilter::Finished, &Done));
        assert!(state_matches(StateFilter::Failed, &Failed("x".into())));
        assert!(state_matches(StateFilter::Failed, &Cancelled));
        assert!(!state_matches(StateFilter::Failed, &Done));
    }

    #[test]
    fn search_by_name_host_and_url() {
        let j = job(
            1,
            "Holiday.Photos.ZIP",
            "https://Files.Example.org/a/b.zip",
            JobStatus::Done,
            1,
            1,
        );
        assert!(matches(&j, StateFilter::All, SearchScope::Name, "photos"));
        assert!(!matches(&j, StateFilter::All, SearchScope::Name, "example"));
        assert!(matches(
            &j,
            StateFilter::All,
            SearchScope::Host,
            "EXAMPLE.org"
        ));
        assert!(matches(&j, StateFilter::All, SearchScope::Url, "/a/b"));
        assert!(
            matches(&j, StateFilter::All, SearchScope::Name, "   "),
            "blank search shows everything"
        );
        assert!(
            !matches(&j, StateFilter::Running, SearchScope::Name, ""),
            "the state filter still applies"
        );
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_of("http://a.test:8080/x"), "a.test");
        assert_eq!(host_of("not a url"), "");
    }

    #[test]
    fn totals_add_up() {
        let mut running = job(1, "a", "http://a.test/a", JobStatus::Downloading, 400, 1000);
        running.parts = vec![
            PartProgress {
                index: 0,
                bytes_done: 0,
                total: 0,
                status: PartStatus::Downloading,
            },
            PartProgress {
                index: 1,
                bytes_done: 0,
                total: 0,
                status: PartStatus::Done,
            },
            PartProgress {
                index: 2,
                bytes_done: 0,
                total: 0,
                status: PartStatus::Retrying { attempt: 1, max: 3 },
            },
        ];
        let jobs = vec![
            running,
            job(2, "b", "http://b.test/b", JobStatus::Done, 500, 500),
            job(3, "c", "http://c.test/c", JobStatus::Paused, 100, 300),
            job(
                4,
                "d",
                "http://d.test/d",
                JobStatus::Failed("x".into()),
                10,
                50,
            ),
        ];
        let t = Totals::compute(&jobs, |id| if id == Uuid::from_u128(1) { 200 } else { 0 });
        assert_eq!(t.links, 4);
        assert_eq!((t.running, t.finished), (1, 1));
        assert_eq!(t.total_bytes, 1850);
        assert_eq!(t.loaded_bytes, 1010);
        assert_eq!(
            t.remaining_bytes,
            600 + 200,
            "running and paused count, failed and done do not"
        );
        assert_eq!(t.bytes_per_sec, 200);
        assert_eq!(t.eta_secs, Some(4));
        assert_eq!(t.connections, 2, "a downloading and a retrying part");
    }

    #[test]
    fn totals_without_speed_have_no_eta() {
        let t = Totals::compute(
            &[job(1, "a", "http://a.test/a", JobStatus::Paused, 0, 100)],
            |_| 0,
        );
        assert_eq!(t.eta_secs, None);
        assert_eq!(Totals::compute(&[], |_| 0), Totals::default());
    }

    #[test]
    fn a_download_without_parts_counts_one_connection() {
        let t = Totals::compute(
            &[job(1, "a", "http://a.test/a", JobStatus::Downloading, 0, 0)],
            |_| 10,
        );
        assert_eq!(t.connections, 1);
    }
}
