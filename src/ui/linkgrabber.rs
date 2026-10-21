//! Link grabber: decoupled collecting, filtering and confirming of links.
//!
//! The grabber is the staging area between "finding links" and "downloading
//! them": links are collected here (typed by hand, imported from a file or the
//! clipboard, or found by a crawler plugin) without a single byte being
//! transferred. The user filters them by hoster and file type, marks the ones
//! worth downloading and only then transfers them into the download queue.
//!
//! The module is free of any terminal or network code, so the workflow can be
//! tested without a TUI.

use std::collections::BTreeSet;

use url::Url;

use crate::plugin::crawler::CrawledLink;

/// Where a collected link came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkSource {
    /// Typed by hand.
    Manual,
    /// Imported from a link list file (the path is kept for the display).
    File(String),
    /// Pasted from the clipboard.
    Clipboard,
    /// Found by a crawler on the given page.
    Crawl(String),
}

/// A link waiting in the grabber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCandidate {
    pub url: Url,
    /// File name guessed from the URL path.
    pub filename: String,
    /// Host of the URL.
    pub host: String,
    /// Lower-case file extension without the dot ("" if the name has none).
    pub extension: String,
    /// Where the link came from.
    pub source: LinkSource,
    /// Size in bytes, if it is already known.
    pub size: Option<u64>,
    /// The user marked this link for the download.
    pub selected: bool,
}

/// A group of links that were collected together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Where the package came from.
    pub source: LinkSource,
    /// Indices of the candidates in this package.
    pub links: Vec<usize>,
}

/// A row of the grabber list: a package header or one of its links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabberRow {
    /// Header of a package.
    Package(usize),
    /// A link inside a package.
    Link(usize),
}

/// Result of a crawler run for the link grabber.
#[derive(Debug, Clone)]
pub enum GrabberEvent {
    /// The crawler found these links on the page.
    LinksFound { page: Url, links: Vec<CrawledLink> },
    /// The crawl failed.
    CrawlFailed { page: Url, error: String },
}

/// Result of a collection: new links and links that were already known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Added {
    /// Links that were added to the grabber.
    pub added: usize,
    /// Links that were already in the grabber.
    pub duplicates: usize,
}

/// Filters that keep links out of the transfer.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GrabFilter {
    /// Hosts that are excluded.
    pub excluded_hosts: BTreeSet<String>,
    /// File extensions that are excluded.
    pub excluded_extensions: BTreeSet<String>,
}

impl GrabFilter {
    /// True if the host is excluded.
    pub fn excludes_host(&self, host: &str) -> bool {
        self.excluded_hosts.contains(host)
    }

    /// True if the file extension is excluded.
    pub fn excludes_extension(&self, extension: &str) -> bool {
        !extension.is_empty() && self.excluded_extensions.contains(extension)
    }

    /// True if the link is kept out by one of the filters.
    pub fn is_excluded(&self, link: &LinkCandidate) -> bool {
        self.excludes_host(&link.host) || self.excludes_extension(&link.extension)
    }

    /// Excludes the host or includes it again; returns the new state
    /// (`true` = excluded).
    pub fn toggle_host(&mut self, host: &str) -> bool {
        if self.excluded_hosts.remove(host) {
            false
        } else {
            self.excluded_hosts.insert(host.to_string());
            true
        }
    }

    /// Excludes the file extension or includes it again; returns the new state
    /// (`true` = excluded). Extensions without a dot are ignored.
    pub fn toggle_extension(&mut self, extension: &str) -> bool {
        if extension.is_empty() {
            return false;
        }
        if self.excluded_extensions.remove(extension) {
            false
        } else {
            self.excluded_extensions
                .insert(extension.to_ascii_lowercase());
            true
        }
    }
}

/// Number of links, marked links and filtered links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GrabberCounts {
    /// Links in the grabber.
    pub total: usize,
    /// Marked links that are not filtered out.
    pub selected: usize,
    /// Links that are filtered out.
    pub filtered: usize,
}

/// Staging area for the collected links.
#[derive(Debug, Default)]
pub struct LinkGrabber {
    links: Vec<LinkCandidate>,
    packages: Vec<Package>,
    filter: GrabFilter,
}

impl LinkGrabber {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn links(&self) -> &[LinkCandidate] {
        &self.links
    }

    pub fn packages(&self) -> &[Package] {
        &self.packages
    }

    pub fn filter(&self) -> &GrabFilter {
        &self.filter
    }

    pub fn filter_mut(&mut self) -> &mut GrabFilter {
        &mut self.filter
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// Collects links; links that are already known are counted as duplicates
    /// and not added twice.
    pub fn add_links(&mut self, source: LinkSource, urls: impl IntoIterator<Item = Url>) -> Added {
        let mut added = Added::default();
        let mut package_links = Vec::new();
        for url in urls {
            if self.links.iter().any(|l| l.url == url) {
                added.duplicates += 1;
                continue;
            }
            let filename = filename_from_url(&url);
            let extension = extension_of(&filename);
            let host = url.host_str().unwrap_or_default().to_string();
            let index = self.links.len();
            self.links.push(LinkCandidate {
                url,
                filename,
                host,
                extension,
                source: source.clone(),
                size: None,
                selected: true,
            });
            package_links.push(index);
            added.added += 1;
        }
        if !package_links.is_empty() {
            self.packages.push(Package {
                source,
                links: package_links,
            });
        }
        added
    }

    /// Collects the links a crawler found on a page.
    pub fn add_crawled(&mut self, page: &Url, links: Vec<CrawledLink>) -> Added {
        let urls = links.into_iter().map(|l| l.url).collect::<Vec<_>>();
        self.add_links(LinkSource::Crawl(page.to_string()), urls)
    }

    pub fn link(&self, index: usize) -> Option<&LinkCandidate> {
        self.links.get(index)
    }

    /// The rows of the list: one header per package, then its links.
    pub fn rows(&self) -> Vec<GrabberRow> {
        let mut rows = Vec::with_capacity(self.links.len() + self.packages.len());
        for (i, package) in self.packages.iter().enumerate() {
            rows.push(GrabberRow::Package(i));
            for &link in &package.links {
                rows.push(GrabberRow::Link(link));
            }
        }
        rows
    }

    /// Marks the link or unmarks it.
    pub fn toggle_link(&mut self, index: usize) {
        if let Some(link) = self.links.get_mut(index) {
            link.selected = !link.selected;
        }
    }

    /// Marks all links of the package; if all of them are marked, they are
    /// unmarked instead.
    pub fn toggle_package(&mut self, package: usize) {
        let Some(pkg) = self.packages.get(package) else {
            return;
        };
        let all = pkg.links.iter().all(|&i| self.links[i].selected);
        for &i in &pkg.links {
            self.links[i].selected = !all;
        }
    }

    /// Marks (or unmarks) every link.
    pub fn select_all(&mut self, selected: bool) {
        for link in &mut self.links {
            link.selected = selected;
        }
    }

    /// The marked links that are not filtered out.
    pub fn selected_links(&self) -> Vec<&LinkCandidate> {
        self.links
            .iter()
            .filter(|l| l.selected && !self.filter.is_excluded(l))
            .collect()
    }

    /// Takes the marked links out of the grabber: they are transferred into the
    /// download queue and therefore leave the staging area.
    pub fn take_selected(&mut self) -> Vec<Url> {
        let taken: Vec<usize> = self
            .links
            .iter()
            .enumerate()
            .filter(|(_, l)| l.selected && !self.filter.is_excluded(l))
            .map(|(i, _)| i)
            .collect();
        if taken.is_empty() {
            return Vec::new();
        }
        let urls = taken.iter().map(|&i| self.links[i].url.clone()).collect();
        self.remove_indices(&taken);
        urls
    }

    /// Removes a single link.
    pub fn remove_link(&mut self, index: usize) -> Option<LinkCandidate> {
        if index >= self.links.len() {
            return None;
        }
        let link = self.links[index].clone();
        self.remove_indices(&[index]);
        Some(link)
    }

    /// Removes a package with all its links.
    pub fn remove_package(&mut self, package: usize) -> Option<Package> {
        let pkg = self.packages.get(package)?.clone();
        self.remove_indices(&pkg.links);
        Some(pkg)
    }

    /// Removes every link and package.
    pub fn clear(&mut self) {
        self.links.clear();
        self.packages.clear();
    }

    /// The numbers shown in the status line.
    pub fn counts(&self) -> GrabberCounts {
        GrabberCounts {
            total: self.links.len(),
            selected: self
                .links
                .iter()
                .filter(|l| l.selected && !self.filter.is_excluded(l))
                .count(),
            filtered: self
                .links
                .iter()
                .filter(|l| self.filter.is_excluded(l))
                .count(),
        }
    }

    /// Removes the links with the given indices and repairs the package
    /// references.
    fn remove_indices(&mut self, indices: &[usize]) {
        let removed: BTreeSet<usize> = indices.iter().copied().collect();
        let mut new_index = vec![None; self.links.len()];
        let mut kept = Vec::with_capacity(self.links.len());
        for (i, link) in self.links.iter().enumerate() {
            if removed.contains(&i) {
                continue;
            }
            new_index[i] = Some(kept.len());
            kept.push(link.clone());
        }
        self.links = kept;
        for package in &mut self.packages {
            package.links = package
                .links
                .iter()
                .filter_map(|i| new_index.get(*i).copied().flatten())
                .collect();
        }
        self.packages.retain(|p| !p.links.is_empty());
    }
}

/// The file name of a URL (last path segment), `"download.bin"` if there is none.
pub fn filename_from_url(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .unwrap_or("download.bin")
        .to_string()
}

/// The lower-case file extension of a name without the dot ("" if there is none).
pub fn extension_of(filename: &str) -> String {
    match filename.rfind('.') {
        Some(i) if i > 0 && i + 1 < filename.len() => filename[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn file_name_and_extension_come_from_the_url() {
        assert_eq!(
            filename_from_url(&url("http://a.test/dir/file.bin")),
            "file.bin"
        );
        assert_eq!(
            filename_from_url(&url("http://a.test/dir/")),
            "download.bin"
        );
        assert_eq!(filename_from_url(&url("http://a.test/")), "download.bin");
        assert_eq!(extension_of("archive.tar.gz"), "gz");
        assert_eq!(extension_of("noextension"), "");
        assert_eq!(extension_of(".hidden"), "");
        assert_eq!(extension_of("trailing."), "");
        assert_eq!(extension_of("Movie.MKV"), "mkv");
    }

    #[test]
    fn collected_links_are_marked_and_grouped_in_a_package() {
        let mut grabber = LinkGrabber::new();
        let added = grabber.add_links(
            LinkSource::Manual,
            [url("http://a.test/1.bin"), url("http://b.test/2.bin")],
        );
        assert_eq!(added.added, 2);
        assert_eq!(added.duplicates, 0);
        assert_eq!(grabber.packages().len(), 1);
        assert_eq!(grabber.packages()[0].links.len(), 2);
        assert!(grabber.links().iter().all(|l| l.selected));
        assert_eq!(grabber.links()[0].filename, "1.bin");
        assert_eq!(grabber.links()[0].extension, "bin");
        assert_eq!(grabber.links()[0].host, "a.test");
        assert_eq!(grabber.links()[1].host, "b.test");
    }

    #[test]
    fn duplicate_links_are_collected_only_once() {
        let mut grabber = LinkGrabber::new();
        grabber.add_links(LinkSource::Manual, [url("http://a.test/1.bin")]);
        let added = grabber.add_links(
            LinkSource::Clipboard,
            [url("http://a.test/1.bin"), url("http://a.test/2.bin")],
        );
        assert_eq!(added.added, 1);
        assert_eq!(added.duplicates, 1);
        assert_eq!(grabber.links().len(), 2);
        assert_eq!(grabber.packages().len(), 2);
    }

    #[test]
    fn every_collection_creates_its_own_package() {
        let mut grabber = LinkGrabber::new();
        grabber.add_links(LinkSource::Manual, [url("http://a.test/1.bin")]);
        grabber.add_links(LinkSource::File("/tmp/links.txt".into()), []);
        // An empty collection does not create a package.
        assert_eq!(grabber.packages().len(), 1);
    }

    #[test]
    fn crawled_links_are_collected_as_a_package() {
        let mut grabber = LinkGrabber::new();
        let page = url("http://a.test/page.html");
        let links = vec![
            CrawledLink {
                url: url("http://a.test/f1.bin"),
                title: Some("one".into()),
            },
            CrawledLink {
                url: url("http://a.test/f2.bin"),
                title: None,
            },
        ];
        let added = grabber.add_crawled(&page, links);
        assert_eq!(added.added, 2);
        assert_eq!(
            grabber.packages()[0].source,
            LinkSource::Crawl(page.to_string())
        );
        assert_eq!(grabber.rows().len(), 3);
        assert_eq!(grabber.rows()[0], GrabberRow::Package(0));
        assert_eq!(grabber.rows()[1], GrabberRow::Link(0));
    }

    #[test]
    fn host_and_type_filters_exclude_links() {
        let mut grabber = LinkGrabber::new();
        grabber.add_links(
            LinkSource::Manual,
            [
                url("http://a.test/1.bin"),
                url("http://a.test/2.html"),
                url("http://b.test/3.bin"),
            ],
        );

        assert!(grabber.filter_mut().toggle_host("a.test"));
        let counts = grabber.counts();
        assert_eq!(counts.total, 3);
        assert_eq!(counts.filtered, 2);
        assert_eq!(counts.selected, 1);

        assert!(grabber.filter_mut().toggle_extension("bin"));
        // The link of b.test is excluded by the type filter now.
        assert_eq!(grabber.counts().filtered, 3);
        assert_eq!(grabber.counts().selected, 0);

        // Toggling again includes them again.
        assert!(!grabber.filter_mut().toggle_extension("bin"));
        assert!(!grabber.filter_mut().toggle_host("a.test"));
        assert_eq!(grabber.counts().filtered, 0);
        assert_eq!(grabber.counts().selected, 3);
    }

    #[test]
    fn filtered_links_are_not_transferred() {
        let mut grabber = LinkGrabber::new();
        grabber.add_links(
            LinkSource::Manual,
            [url("http://a.test/1.bin"), url("http://b.test/2.bin")],
        );
        grabber.filter_mut().toggle_host("b.test");
        let urls = grabber.take_selected();
        assert_eq!(urls, vec![url("http://a.test/1.bin")]);
        // The transferred link left the grabber, the filtered one stayed.
        assert_eq!(grabber.links().len(), 1);
        assert_eq!(grabber.links()[0].host, "b.test");
    }

    #[test]
    fn marking_links_and_packages() {
        let mut grabber = LinkGrabber::new();
        grabber.add_links(
            LinkSource::Manual,
            [url("http://a.test/1.bin"), url("http://b.test/2.bin")],
        );
        grabber.toggle_link(0);
        assert!(!grabber.links()[0].selected);
        assert!(grabber.links()[1].selected);

        // Toggling the package marks all of its links.
        grabber.toggle_package(0);
        assert!(grabber.links().iter().all(|l| l.selected));
        // Toggling it again unmarks them.
        grabber.toggle_package(0);
        assert!(grabber.links().iter().all(|l| !l.selected));

        grabber.select_all(true);
        assert_eq!(grabber.counts().selected, 2);
        grabber.select_all(false);
        assert_eq!(grabber.counts().selected, 0);
    }

    #[test]
    fn removing_a_link_repairs_the_packages() {
        let mut grabber = LinkGrabber::new();
        grabber.add_links(
            LinkSource::Manual,
            [url("http://a.test/1.bin"), url("http://a.test/2.bin")],
        );
        grabber.add_links(LinkSource::Clipboard, [url("http://b.test/3.bin")]);

        let removed = grabber.remove_link(0).unwrap();
        assert_eq!(removed.host, "a.test");
        assert_eq!(grabber.links().len(), 2);
        // The first package keeps its remaining link, the indices are repaired.
        assert_eq!(grabber.packages()[0].links, vec![0]);
        assert_eq!(grabber.packages()[1].links, vec![1]);
        assert_eq!(grabber.links()[0].host, "a.test");
        assert_eq!(grabber.links()[1].host, "b.test");
    }

    #[test]
    fn removing_a_package_removes_its_links() {
        let mut grabber = LinkGrabber::new();
        grabber.add_links(
            LinkSource::Manual,
            [url("http://a.test/1.bin"), url("http://a.test/2.bin")],
        );
        grabber.add_links(LinkSource::Clipboard, [url("http://b.test/3.bin")]);

        grabber.remove_package(0);
        assert_eq!(grabber.links().len(), 1);
        assert_eq!(grabber.links()[0].host, "b.test");
        assert_eq!(grabber.packages().len(), 1);
        assert_eq!(grabber.packages()[0].source, LinkSource::Clipboard);
    }

    #[test]
    fn taking_the_selected_links_empties_the_grabber() {
        let mut grabber = LinkGrabber::new();
        grabber.add_links(
            LinkSource::Manual,
            [url("http://a.test/1.bin"), url("http://a.test/2.bin")],
        );
        let urls = grabber.take_selected();
        assert_eq!(urls.len(), 2);
        assert!(grabber.is_empty());
        assert!(grabber.packages().is_empty());
        // Nothing left to take.
        assert!(grabber.take_selected().is_empty());
    }
}
