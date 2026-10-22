//! View logic of the window without any drawing: which downloads are shown,
//! table columns, sorting, package grouping, and overview panel totals.
//! Pure functions, unit-tested.

use std::cmp::Ordering;
use uuid::Uuid;

use crate::download::{JobProgress, JobStatus, PartStatus};

/// Columns available in the downloads table.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ColumnId {
    Name,
    Size,
    Host,
    Connection,
    Status,
    Speed,
    Eta,
    Loaded,
    Progress,
    SaveTo,
    Comment,
}

impl ColumnId {
    pub const ALL: [ColumnId; 11] = [
        ColumnId::Name,
        ColumnId::Size,
        ColumnId::Host,
        ColumnId::Connection,
        ColumnId::Status,
        ColumnId::Speed,
        ColumnId::Eta,
        ColumnId::Loaded,
        ColumnId::Progress,
        ColumnId::SaveTo,
        ColumnId::Comment,
    ];

    /// The i18n translation key for this column's header.
    pub fn title_key(&self) -> &'static str {
        match self {
            ColumnId::Name => "gui.col.name",
            ColumnId::Size => "gui.col.size",
            ColumnId::Host => "gui.col.host",
            ColumnId::Connection => "gui.col.connection",
            ColumnId::Status => "gui.col.status",
            ColumnId::Speed => "gui.col.speed",
            ColumnId::Eta => "gui.col.eta",
            ColumnId::Loaded => "gui.col.loaded",
            ColumnId::Progress => "gui.col.progress",
            ColumnId::SaveTo => "gui.col.save_to",
            ColumnId::Comment => "gui.col.comment",
        }
    }

    /// Default width in points.
    pub fn default_width(&self) -> f32 {
        match self {
            ColumnId::Name => 220.0,
            ColumnId::Size => 80.0,
            ColumnId::Host => 110.0,
            ColumnId::Connection => 85.0,
            ColumnId::Status => 95.0,
            ColumnId::Speed => 85.0,
            ColumnId::Eta => 75.0,
            ColumnId::Loaded => 80.0,
            ColumnId::Progress => 110.0,
            ColumnId::SaveTo => 140.0,
            ColumnId::Comment => 120.0,
        }
    }
}

/// Sort direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SortDirection {
    #[default]
    Ascending,
    Descending,
}

impl SortDirection {
    pub fn reverse(self) -> Self {
        match self {
            SortDirection::Ascending => SortDirection::Descending,
            SortDirection::Descending => SortDirection::Ascending,
        }
    }
}

/// Active sort column and direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SortConfig {
    pub column: ColumnId,
    pub direction: SortDirection,
}

impl Default for SortConfig {
    fn default() -> Self {
        Self {
            column: ColumnId::Name,
            direction: SortDirection::Ascending,
        }
    }
}

/// Visibility configuration for each column in the downloads table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnConfig {
    pub name: bool,
    pub size: bool,
    pub host: bool,
    pub connection: bool,
    pub status: bool,
    pub speed: bool,
    pub eta: bool,
    pub loaded: bool,
    pub progress: bool,
    pub save_to: bool,
    pub comment: bool,
}

impl Default for ColumnConfig {
    fn default() -> Self {
        Self {
            name: true,
            size: true,
            host: true,
            connection: true,
            status: true,
            speed: true,
            eta: true,
            loaded: true,
            progress: true,
            save_to: true,
            comment: true,
        }
    }
}

impl ColumnConfig {
    pub fn is_visible(&self, col: ColumnId) -> bool {
        match col {
            ColumnId::Name => self.name,
            ColumnId::Size => self.size,
            ColumnId::Host => self.host,
            ColumnId::Connection => self.connection,
            ColumnId::Status => self.status,
            ColumnId::Speed => self.speed,
            ColumnId::Eta => self.eta,
            ColumnId::Loaded => self.loaded,
            ColumnId::Progress => self.progress,
            ColumnId::SaveTo => self.save_to,
            ColumnId::Comment => self.comment,
        }
    }

    pub fn set_visible(&mut self, col: ColumnId, visible: bool) {
        match col {
            ColumnId::Name => self.name = visible,
            ColumnId::Size => self.size = visible,
            ColumnId::Host => self.host = visible,
            ColumnId::Connection => self.connection = visible,
            ColumnId::Status => self.status = visible,
            ColumnId::Speed => self.speed = visible,
            ColumnId::Eta => self.eta = visible,
            ColumnId::Loaded => self.loaded = visible,
            ColumnId::Progress => self.progress = visible,
            ColumnId::SaveTo => self.save_to = visible,
            ColumnId::Comment => self.comment = visible,
        }
    }

    pub fn toggle(&mut self, col: ColumnId) {
        let current = self.is_visible(col);
        self.set_visible(col, !current);
    }
}

/// A download item row in the GUI table with all column-relevant fields.
#[derive(Clone, Debug)]
pub struct GuiItem {
    pub id: Uuid,
    pub name: String,
    pub size: u64,
    pub host: String,
    pub connection: String,
    pub status: JobStatus,
    pub speed: u64,
    pub eta: Option<u64>,
    pub loaded: u64,
    pub progress: Option<f32>,
    pub save_to: String,
    pub comment: String,
    pub raw: JobProgress,
}

impl GuiItem {
    pub fn from_job(
        job: &JobProgress,
        speed: u64,
        save_to: impl Into<String>,
        comment: impl Into<String>,
        connection: impl Into<String>,
    ) -> Self {
        let name = if job.filename.is_empty() {
            job.url.clone()
        } else {
            job.filename.clone()
        };
        let host = host_of(&job.url);
        let remaining = job.total_bytes.saturating_sub(job.bytes_done);
        let eta = (speed > 0 && job.total_bytes > 0).then(|| remaining.div_ceil(speed));
        let progress = if matches!(job.status, JobStatus::Done) {
            Some(1.0)
        } else if job.total_bytes > 0 {
            Some((job.bytes_done as f64 / job.total_bytes as f64).min(1.0) as f32)
        } else {
            None
        };
        Self {
            id: job.id,
            name,
            size: job.total_bytes,
            host,
            connection: connection.into(),
            status: job.status.clone(),
            speed,
            eta,
            loaded: job.bytes_done,
            progress,
            save_to: save_to.into(),
            comment: comment.into(),
            raw: job.clone(),
        }
    }
}

/// A package grouping multiple download items.
#[derive(Clone, Debug)]
pub struct GuiPackage {
    pub id: String,
    pub name: String,
    pub save_to: String,
    pub comment: String,
    pub items: Vec<GuiItem>,
}

impl GuiPackage {
    pub fn new(
        name: impl Into<String>,
        save_to: impl Into<String>,
        comment: impl Into<String>,
    ) -> Self {
        let name = name.into();
        Self {
            id: name.clone(),
            name,
            save_to: save_to.into(),
            comment: comment.into(),
            items: Vec::new(),
        }
    }

    pub fn total_size(&self) -> u64 {
        self.items.iter().map(|i| i.size).sum()
    }

    pub fn loaded_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.loaded).sum()
    }

    pub fn total_speed(&self) -> u64 {
        self.items.iter().map(|i| i.speed).sum()
    }

    pub fn eta(&self) -> Option<u64> {
        let speed = self.total_speed();
        if speed == 0 {
            return None;
        }
        let total = self.total_size();
        let loaded = self.loaded_bytes();
        if total == 0 || loaded >= total {
            None
        } else {
            Some((total - loaded).div_ceil(speed))
        }
    }

    pub fn progress(&self) -> Option<f32> {
        let total = self.total_size();
        if total == 0 {
            if self.items.is_empty() {
                None
            } else if self
                .items
                .iter()
                .all(|i| matches!(i.status, JobStatus::Done))
            {
                Some(1.0)
            } else {
                None
            }
        } else {
            let loaded = self.loaded_bytes();
            Some((loaded as f64 / total as f64).min(1.0) as f32)
        }
    }

    pub fn status_summary(&self) -> JobStatus {
        if self.items.is_empty() {
            return JobStatus::Queued;
        }
        if self
            .items
            .iter()
            .all(|i| matches!(i.status, JobStatus::Done))
        {
            return JobStatus::Done;
        }
        if self
            .items
            .iter()
            .any(|i| matches!(i.status, JobStatus::Downloading))
        {
            return JobStatus::Downloading;
        }
        if self
            .items
            .iter()
            .any(|i| matches!(i.status, JobStatus::Merging))
        {
            return JobStatus::Merging;
        }
        if self
            .items
            .iter()
            .any(|i| matches!(i.status, JobStatus::Resolving))
        {
            return JobStatus::Resolving;
        }
        if self
            .items
            .iter()
            .any(|i| matches!(i.status, JobStatus::Paused))
        {
            return JobStatus::Paused;
        }
        if let Some(failed) = self.items.iter().find_map(|i| match &i.status {
            JobStatus::Failed(e) => Some(e.clone()),
            _ => None,
        }) {
            return JobStatus::Failed(failed);
        }
        if self
            .items
            .iter()
            .any(|i| matches!(i.status, JobStatus::Cancelled))
        {
            return JobStatus::Cancelled;
        }
        JobStatus::Queued
    }
}

/// Comparison function for `GuiItem` by column and direction.
pub fn compare_items(a: &GuiItem, b: &GuiItem, sort: SortConfig) -> Ordering {
    let ord = match sort.column {
        ColumnId::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        ColumnId::Size => a.size.cmp(&b.size),
        ColumnId::Host => a.host.to_lowercase().cmp(&b.host.to_lowercase()),
        ColumnId::Connection => a
            .connection
            .to_lowercase()
            .cmp(&b.connection.to_lowercase()),
        ColumnId::Status => status_rank(&a.status).cmp(&status_rank(&b.status)),
        ColumnId::Speed => a.speed.cmp(&b.speed),
        ColumnId::Eta => a.eta.unwrap_or(u64::MAX).cmp(&b.eta.unwrap_or(u64::MAX)),
        ColumnId::Loaded => a.loaded.cmp(&b.loaded),
        ColumnId::Progress => {
            let p_a = a.progress.unwrap_or(-1.0);
            let p_b = b.progress.unwrap_or(-1.0);
            p_a.partial_cmp(&p_b).unwrap_or(Ordering::Equal)
        }
        ColumnId::SaveTo => a.save_to.to_lowercase().cmp(&b.save_to.to_lowercase()),
        ColumnId::Comment => a.comment.to_lowercase().cmp(&b.comment.to_lowercase()),
    };
    match sort.direction {
        SortDirection::Ascending => ord,
        SortDirection::Descending => ord.reverse(),
    }
}

/// Comparison function for `GuiPackage` by column and direction.
pub fn compare_packages(a: &GuiPackage, b: &GuiPackage, sort: SortConfig) -> Ordering {
    let ord = match sort.column {
        ColumnId::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        ColumnId::Size => a.total_size().cmp(&b.total_size()),
        ColumnId::Host => a.items.len().cmp(&b.items.len()),
        ColumnId::Connection => a.name.cmp(&b.name),
        ColumnId::Status => {
            let sa = a.status_summary();
            let sb = b.status_summary();
            status_rank(&sa).cmp(&status_rank(&sb))
        }
        ColumnId::Speed => a.total_speed().cmp(&b.total_speed()),
        ColumnId::Eta => a
            .eta()
            .unwrap_or(u64::MAX)
            .cmp(&b.eta().unwrap_or(u64::MAX)),
        ColumnId::Loaded => a.loaded_bytes().cmp(&b.loaded_bytes()),
        ColumnId::Progress => {
            let p_a = a.progress().unwrap_or(-1.0);
            let p_b = b.progress().unwrap_or(-1.0);
            p_a.partial_cmp(&p_b).unwrap_or(Ordering::Equal)
        }
        ColumnId::SaveTo => a.save_to.to_lowercase().cmp(&b.save_to.to_lowercase()),
        ColumnId::Comment => a.comment.to_lowercase().cmp(&b.comment.to_lowercase()),
    };
    match sort.direction {
        SortDirection::Ascending => ord,
        SortDirection::Descending => ord.reverse(),
    }
}

fn status_rank(s: &JobStatus) -> u8 {
    match s {
        JobStatus::Downloading => 0,
        JobStatus::Merging => 1,
        JobStatus::Resolving => 2,
        JobStatus::Queued => 3,
        JobStatus::Paused => 4,
        JobStatus::Done => 5,
        JobStatus::Cancelled => 6,
        JobStatus::Failed(_) => 7,
    }
}

/// Groups items into packages and sorts both packages and child items according to `sort`.
pub fn build_and_sort_packages(
    mut items: Vec<GuiItem>,
    sort: SortConfig,
    default_pkg_name: &str,
) -> Vec<GuiPackage> {
    // Default grouping: items with same save_to / package grouping if present,
    // or single default package if all items belong to default.
    // For now we group by package name if item has one, or default package name.
    let mut packages: Vec<GuiPackage> = Vec::new();

    // Preserve insertion grouping: map by name
    for item in items.drain(..) {
        let pkg_name = if item.comment.starts_with("pkg:") {
            item.comment.trim_start_matches("pkg:").to_string()
        } else {
            default_pkg_name.to_string()
        };

        if let Some(pkg) = packages.iter_mut().find(|p| p.name == pkg_name) {
            pkg.items.push(item);
        } else {
            let mut pkg = GuiPackage::new(pkg_name, item.save_to.clone(), item.comment.clone());
            pkg.items.push(item);
            packages.push(pkg);
        }
    }

    // Sort items within each package
    for pkg in &mut packages {
        pkg.items.sort_by(|a, b| compare_items(a, b, sort));
    }

    // Sort packages
    packages.sort_by(|a, b| compare_packages(a, b, sort));

    packages
}

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
    fn column_config_visibility_toggle() {
        let mut cfg = ColumnConfig::default();
        assert!(cfg.is_visible(ColumnId::Name));
        assert!(cfg.is_visible(ColumnId::Comment));

        cfg.toggle(ColumnId::Comment);
        assert!(!cfg.is_visible(ColumnId::Comment));

        cfg.set_visible(ColumnId::Comment, true);
        assert!(cfg.is_visible(ColumnId::Comment));

        for col in ColumnId::ALL {
            assert!(cfg.is_visible(col));
        }
    }

    #[test]
    fn gui_item_conversion_and_eta() {
        let j = job(
            1,
            "file.zip",
            "https://example.com/file.zip",
            JobStatus::Downloading,
            500,
            1000,
        );
        let item = GuiItem::from_job(&j, 100, "/tmp/downloads", "manual", "Tor");
        assert_eq!(item.name, "file.zip");
        assert_eq!(item.host, "example.com");
        assert_eq!(item.size, 1000);
        assert_eq!(item.loaded, 500);
        assert_eq!(item.speed, 100);
        assert_eq!(item.eta, Some(5)); // (1000 - 500) / 100
        assert_eq!(item.progress, Some(0.5));
        assert_eq!(item.save_to, "/tmp/downloads");
        assert_eq!(item.comment, "manual");
        assert_eq!(item.connection, "Tor");
    }

    #[test]
    fn item_sorting() {
        let j1 = job(
            1,
            "beta.iso",
            "https://b.com/beta.iso",
            JobStatus::Queued,
            10,
            100,
        );
        let j2 = job(
            2,
            "alpha.iso",
            "https://a.com/alpha.iso",
            JobStatus::Downloading,
            50,
            200,
        );

        let i1 = GuiItem::from_job(&j1, 0, "/dir1", "c1", "Tor");
        let i2 = GuiItem::from_job(&j2, 50, "/dir2", "c2", "Direct");

        // Sort by Name Ascending: alpha comes before beta
        let sort_name_asc = SortConfig {
            column: ColumnId::Name,
            direction: SortDirection::Ascending,
        };
        assert_eq!(compare_items(&i1, &i2, sort_name_asc), Ordering::Greater);

        // Sort by Name Descending: beta comes before alpha
        let sort_name_desc = SortConfig {
            column: ColumnId::Name,
            direction: SortDirection::Descending,
        };
        assert_eq!(compare_items(&i1, &i2, sort_name_desc), Ordering::Less);

        // Sort by Size Ascending: 100 < 200
        let sort_size_asc = SortConfig {
            column: ColumnId::Size,
            direction: SortDirection::Ascending,
        };
        assert_eq!(compare_items(&i1, &i2, sort_size_asc), Ordering::Less);
    }

    #[test]
    fn package_grouping_and_summaries() {
        let j1 = job(
            1,
            "part1.rar",
            "https://host.com/part1.rar",
            JobStatus::Done,
            100,
            100,
        );
        let j2 = job(
            2,
            "part2.rar",
            "https://host.com/part2.rar",
            JobStatus::Downloading,
            40,
            100,
        );

        let i1 = GuiItem::from_job(&j1, 0, "/save", "pkg:Archive", "Tor");
        let i2 = GuiItem::from_job(&j2, 20, "/save", "pkg:Archive", "Tor");

        let packages = build_and_sort_packages(vec![i1, i2], SortConfig::default(), "Default");
        assert_eq!(packages.len(), 1);
        let pkg = &packages[0];
        assert_eq!(pkg.name, "Archive");
        assert_eq!(pkg.items.len(), 2);
        assert_eq!(pkg.total_size(), 200);
        assert_eq!(pkg.loaded_bytes(), 140);
        assert_eq!(pkg.total_speed(), 20);
        assert_eq!(pkg.status_summary(), JobStatus::Downloading);
        assert_eq!(pkg.eta(), Some(3)); // (200 - 140) / 20 = 3
        assert_eq!(pkg.progress(), Some(0.7)); // 140 / 200 = 0.7
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
