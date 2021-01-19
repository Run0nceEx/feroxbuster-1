use super::*;
use crate::{
    client,
    event_handlers::Command::UpdateUsizeField,
    scanner::{send_report, should_filter_response, try_recursion},
    send_command,
    statistics::StatField::{LinksExtracted, TotalExpected},
    utils::{format_url, make_request},
    CommandSender,
};
use anyhow::{bail, Context, Result};
use reqwest::{StatusCode, Url};
use std::collections::HashSet;

/// Whether an active scan is recursive or not
#[derive(Debug)]
enum RecursionStatus {
    /// Scan is recursive
    Recursive,

    /// Scan is not recursive
    NotRecursive,
}

/// Handles all logic related to extracting links from requested source code
#[derive(Debug)]
pub struct Extractor<'a> {
    /// `LINKFINDER_REGEX` as a regex::Regex type
    pub(super) links_regex: Regex,

    /// `ROBOTS_TXT_REGEX` as a regex::Regex type
    pub(super) robots_regex: Regex,

    /// Response from which to extract links
    pub(super) response: Option<&'a FeroxResponse>,

    /// Response from which to extract links
    pub(super) url: String,

    /// Whether or not to try recursion
    pub(super) config: &'a Configuration,

    /// transmitter to the mpsc that handles statistics gathering
    pub(super) tx_stats: CommandSender,

    /// transmitter to the mpsc that handles recursive scan calls
    pub(super) tx_recursion: UnboundedSender<String>,

    /// transmitter to the mpsc that handles reporting information to the user
    pub(super) tx_reporter: CommandSender,

    /// list of urls that will be added to when new urls are extracted
    pub(super) scanned_urls: &'a FeroxScans,

    /// depth at which the scan was started
    pub(super) depth: usize,

    /// copy of Stats object
    pub(super) stats: Arc<Stats>,

    /// type of extraction to be performed
    pub(super) target: ExtractionTarget,
}

/// Extractor implementation
impl<'a> Extractor<'a> {
    /// business logic that handles getting links from a normal http body response
    pub async fn extract(&self) -> Result<()> {
        let links = match self.target {
            ExtractionTarget::ResponseBody => self.extract_from_body().await?,
            ExtractionTarget::RobotsTxt => self.extract_from_robots().await?,
        };

        let recursive = if self.config.no_recursion {
            RecursionStatus::NotRecursive
        } else {
            RecursionStatus::Recursive
        };

        for link in links {
            // todo rename get_feroxresponse_from_link
            let mut resp = match self.request_link(&link).await {
                Ok(resp) => resp,
                Err(_) => continue,
            };

            // filter if necessary
            if should_filter_response(&resp, self.tx_stats.clone()) {
                continue;
            }

            if resp.is_file() {
                // very likely a file, simply request and report
                log::debug!("Extracted file: {}", resp);

                self.scanned_urls
                    .add_file_scan(&resp.url().to_string(), self.stats.clone());

                send_report(self.tx_reporter.clone(), resp);

                continue;
            }

            if matches!(recursive, RecursionStatus::Recursive) {
                log::debug!("Extracted Directory: {}", resp);

                if !resp.url().as_str().ends_with('/')
                    && (resp.status().is_success()
                        || matches!(resp.status(), &StatusCode::FORBIDDEN))
                {
                    // if the url doesn't end with a /
                    // and the response code is either a 2xx or 403

                    // since all of these are 2xx or 403, recursion is only attempted if the
                    // url ends in a /. I am actually ok with adding the slash and not
                    // adding it, as both have merit.  Leaving it in for now to see how
                    // things turn out (current as of: v1.1.0)
                    resp.set_url(&format!("{}/", resp.url()));
                }

                try_recursion(&resp, self.depth, self.tx_recursion.clone()).await;
            }
        }
        Ok(())
    }

    /// Given a `reqwest::Response`, perform the following actions
    ///   - parse the response's text for links using the linkfinder regex
    ///   - for every link found take its url path and parse each sub-path
    ///     - example: Response contains a link fragment `homepage/assets/img/icons/handshake.svg`
    ///       with a base url of http://localhost, the following urls would be returned:
    ///         - homepage/assets/img/icons/handshake.svg
    ///         - homepage/assets/img/icons/
    ///         - homepage/assets/img/
    ///         - homepage/assets/
    ///         - homepage/
    pub(super) async fn extract_from_body(&self) -> Result<HashSet<String>> {
        log::trace!("enter: get_links");

        let mut links = HashSet::<String>::new();

        let body = self.response.unwrap().text();

        for capture in self.links_regex.captures_iter(&body) {
            // remove single & double quotes from both ends of the capture
            // capture[0] is the entire match, additional capture groups start at [1]
            let link = capture[0].trim_matches(|c| c == '\'' || c == '"');

            match Url::parse(link) {
                Ok(absolute) => {
                    if absolute.domain() != self.response.unwrap().url().domain()
                        || absolute.host() != self.response.unwrap().url().host()
                    {
                        // domains/ips are not the same, don't scan things that aren't part of the original
                        // target url
                        continue;
                    }

                    if self.add_all_sub_paths(absolute.path(), &mut links).is_err() {
                        log::warn!("could not add sub-paths from {} to {:?}", absolute, links);
                    }
                }
                Err(e) => {
                    // this is the expected error that happens when we try to parse a url fragment
                    //     ex: Url::parse("/login") -> Err("relative URL without a base")
                    // while this is technically an error, these are good results for us
                    if e.to_string().contains("relative URL without a base") {
                        if self.add_all_sub_paths(link, &mut links).is_err() {
                            log::warn!("could not add sub-paths from {} to {:?}", link, links);
                        }
                    } else {
                        // unexpected error has occurred
                        log::error!("Could not parse given url: {}", e);
                    }
                }
            }
        }

        self.update_stats(links.len());

        log::trace!("exit: get_links -> {:?}", links);

        Ok(links)
    }

    /// take a url fragment like homepage/assets/img/icons/handshake.svg and
    /// incrementally add
    ///     - homepage/assets/img/icons/
    ///     - homepage/assets/img/
    ///     - homepage/assets/
    ///     - homepage/
    fn add_all_sub_paths(&self, url_path: &str, mut links: &mut HashSet<String>) -> Result<()> {
        log::trace!("enter: add_all_sub_paths({}, {:?})", url_path, links);

        for sub_path in self.get_sub_paths_from_path(url_path) {
            self.add_link_to_set_of_links(&sub_path, &mut links)?;
        }

        log::trace!("exit: add_all_sub_paths");
        Ok(())
    }

    /// Iterate over a given path, return a list of every sub-path found
    ///
    /// example: `path` contains a link fragment `homepage/assets/img/icons/handshake.svg`
    /// the following fragments would be returned:
    ///   - homepage/assets/img/icons/handshake.svg
    ///   - homepage/assets/img/icons/
    ///   - homepage/assets/img/
    ///   - homepage/assets/
    ///   - homepage/
    pub(super) fn get_sub_paths_from_path(&self, path: &str) -> Vec<String> {
        log::trace!("enter: get_sub_paths_from_path({})", path);
        let mut paths = vec![];

        // filter out any empty strings caused by .split
        let mut parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        let length = parts.len();

        for i in 0..length {
            // iterate over all parts of the path
            if parts.is_empty() {
                // pop left us with an empty vector, we're done
                break;
            }

            let mut possible_path = parts.join("/");

            if possible_path.is_empty() {
                // .join can result in an empty string, which we don't need, ignore
                continue;
            }

            if i > 0 {
                // this isn't the last index of the parts array
                // ex: /buried/misc/stupidfile.php
                // this block skips the file but sees all parent folders
                possible_path = format!("{}/", possible_path);
            }

            paths.push(possible_path); // good sub-path found
            parts.pop(); // use .pop() to remove the last part of the path and continue iteration
        }

        log::trace!("exit: get_sub_paths_from_path -> {:?}", paths);
        paths
    }

    /// simple helper to stay DRY, trys to join a url + fragment and add it to the `links` HashSet
    pub(super) fn add_link_to_set_of_links(
        &self,
        link: &str,
        links: &mut HashSet<String>,
    ) -> Result<()> {
        log::trace!("enter: add_link_to_set_of_links({}, {:?})", link, links);

        let old_url = match self.target {
            ExtractionTarget::ResponseBody => self.response.unwrap().url.clone(),
            ExtractionTarget::RobotsTxt => match Url::parse(&self.url) {
                Ok(u) => u,
                Err(e) => {
                    bail!("Could not parse {}: {}", self.url, e);
                }
            },
        };

        let new_url = old_url
            .join(&link)
            .with_context(|| format!("Could not join {} with {}", old_url, link))?;

        links.insert(new_url.to_string());

        log::trace!("exit: add_link_to_set_of_links");

        Ok(())
    }

    /// Wrapper around link extraction logic
    /// currently used in two places:
    ///   - links from response bodies
    ///   - links from robots.txt responses
    ///
    /// general steps taken:
    ///   - create a new Url object based on cli options/args
    ///   - check if the new Url has already been seen/scanned -> None
    ///   - make a request to the new Url ? -> Some(response) : None
    pub(super) async fn request_link(&self, url: &str) -> Result<FeroxResponse> {
        log::trace!("enter: get_feroxresponse_from_link({})", url);

        // create a url based on the given command line options, return None on error
        let new_url = format_url(
            &url,
            &"",
            self.config.add_slash,
            &self.config.queries,
            None,
            self.tx_stats.clone(),
        )?;

        if self
            .scanned_urls
            .get_scan_by_url(&new_url.to_string())
            .is_some()
        {
            //we've seen the url before and don't need to scan again
            log::trace!("exit: get_feroxresponse_from_link -> None");
            bail!("previously seen url");
        }

        // make the request and store the response
        let new_response =
            make_request(&self.config.client, &new_url, self.tx_stats.clone()).await?;

        let new_ferox_response = FeroxResponse::from(new_response, true).await;

        log::trace!(
            "exit: get_feroxresponse_from_link -> {:?}",
            new_ferox_response
        );

        Ok(new_ferox_response)
    }

    /// Entry point to perform link extraction from robots.txt
    ///
    /// `base_url` can have paths and subpaths, however robots.txt will be requested from the
    /// root of the url
    /// given the url:
    ///     http://localhost/stuff/things
    /// this function requests:
    ///     http://localhost/robots.txt
    pub(super) async fn extract_from_robots(&self) -> Result<HashSet<String>> {
        log::trace!("enter: extract_robots_txt");

        let mut links: HashSet<String> = HashSet::new();

        let response = self.request_robots_txt().await?;

        for capture in self.robots_regex.captures_iter(response.text.as_str()) {
            if let Some(new_path) = capture.name("url_path") {
                let mut new_url = Url::parse(&self.url)?;
                new_url.set_path(new_path.as_str());
                if self.add_all_sub_paths(&new_url.path(), &mut links).is_err() {
                    log::warn!("could not add sub-paths from {} to {:?}", new_url, links);
                }
            }
        }

        self.update_stats(links.len());

        log::trace!("exit: extract_robots_txt -> {:?}", links);
        Ok(links)
    }

    /// helper function that simply requests /robots.txt on the given url's base url
    ///
    /// example:
    ///     http://localhost/api/users -> http://localhost/robots.txt
    ///     
    /// The length of the given path has no effect on what's requested; it's always
    /// base url + /robots.txt
    pub(super) async fn request_robots_txt(&self) -> Result<FeroxResponse> {
        log::trace!("enter: get_robots_file");

        // more often than not, domain/robots.txt will redirect to www.domain/robots.txt or something
        // similar; to account for that, create a client that will follow redirects, regardless of
        // what the user specified for the scanning client. Other than redirects, it will respect
        // all other user specified settings
        let follow_redirects = true;

        let proxy = if self.config.proxy.is_empty() {
            None
        } else {
            Some(self.config.proxy.as_str())
        };

        let client = client::initialize(
            self.config.timeout,
            &self.config.user_agent,
            follow_redirects,
            self.config.insecure,
            &self.config.headers,
            proxy,
        );

        let mut url = Url::parse(&self.url)?;
        url.set_path("/robots.txt"); // overwrite existing path with /robots.txt

        let response = make_request(&client, &url, self.tx_stats.clone()).await?;
        let ferox_response = FeroxResponse::from(response, true).await;

        log::trace!("exit: get_robots_file -> {}", ferox_response);
        return Ok(ferox_response);
    }

    /// update total number of links extracted and expected responses
    fn update_stats(&self, num_links: usize) {
        let multiplier = self.config.extensions.len().max(1);

        send_command!(self.tx_stats, UpdateUsizeField(LinksExtracted, num_links));
        send_command!(
            self.tx_stats,
            UpdateUsizeField(TotalExpected, num_links * multiplier)
        );
    }
}
