use core::time::Duration;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::{fs, process};

use clap::Parser as _;
use regex::Regex;
use reqwest::header::{FROM, HeaderValue};
use tokio::sync::mpsc::channel;
use tokio::time::sleep;

use crate::cli::Cli;
use crate::config::Config;
use crate::site::Site;

mod cli;
mod commit_message;
mod config;
mod editor;
mod filename;
mod git;
mod github;
mod http;
mod logger;
mod notification;
mod site;
mod site_store;
mod state_store;

const WAIT_BETWEEN_SAME_HOST: Duration = Duration::from_secs(5);

pub enum ChangeKind {
    Init,
    Changed,
    ContentSame,
}

impl core::fmt::Display for ChangeKind {
    fn fmt(&self, fmt: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Init => fmt.pad("Init"),
            Self::Changed => fmt.pad("Changed"),
            Self::ContentSame => fmt.pad("ContentSame"),
        }
    }
}

#[tokio::main]
async fn main() {
    match Cli::parse() {
        Cli::ExampleConfig => print!("{}", Config::EXAMPLE),
        Cli::JsonSchema => {
            // `yaml-language-server` only works with `draft-07` for some things
            let settings = schemars::generate::SchemaSettings::draft07();
            let schema = schemars::SchemaGenerator::new(settings).into_root_schema_for::<Config>();
            let schema = serde_json::to_string_pretty(&schema).unwrap();
            println!("{schema}");
        }
        Cli::Init => {
            logger::warn(
                "website-stalker init is deprecated. Use `git init && website-stalker example-config > website-stalker.yaml`",
            );
            if git::Repo::new().is_err() {
                git::Repo::init(
                    &std::env::current_dir().expect("Should be run in a valid working directory"),
                );
                println!("Git repository initialized.");
            }
            let from = std::env::var("WEBSITE_STALKER_FROM").ok();
            if Config::load(from).is_err() {
                fs::write("website-stalker.yaml", Config::EXAMPLE)
                    .expect("failed to write example configuration file");
                println!("Example configuration file generated.");
            }
            println!("Init complete.\nNext step: adapt the configuration file to your needs.");
        }
        Cli::Check => {
            logger::warn(
                "website-stalker check is deprecated. website-stalker run also checks the config and runs it when valid.",
            );
            eprintln!("\nConfiguration...");
            let from = std::env::var("WEBSITE_STALKER_FROM").ok();
            match Config::load(from) {
                Ok(_) => eprintln!("ok"),
                Err(err) => {
                    eprintln!(
                        "not ok.\n\n{err:#}\n\nCheck https://github.com/EdJoPaTo/website-stalker for configuration details."
                    );
                    process::exit(1);
                }
            }
        }
        Cli::Run {
            all: _all,
            commit: do_commit,
            from,
            notification_commit_template,
            notifications,
            site_filter,
        } => {
            let site_filter =
                site_filter.map(|regex| Regex::new(&format!("(?i){}", regex.as_str())).unwrap());
            run(
                do_commit,
                from,
                notifications,
                notification_commit_template,
                site_filter.as_ref(),
            )
            .await;
        }
    }
}

#[expect(clippy::too_many_lines)]
async fn run(
    do_commit: bool,
    from: Option<String>,
    notifications: pling::clap::Args,
    notification_commit_template: Option<String>,
    site_filter: Option<&Regex>,
) {
    let config = Config::load(from).expect("failed to load your configuration");
    let from = config
        .from
        .parse::<HeaderValue>()
        .expect("FROM has to be valid");

    let sites = config.get_sites();
    let sites_total = sites.len();
    let sites = sites
        .into_iter()
        .filter(|site| site_filter.is_none_or(|filter| filter.is_match(site.url.as_str())))
        .collect::<Vec<_>>();
    let sites_amount = sites.len();
    if sites.is_empty() {
        logger::error_exit(
            "The site-filter filtered everything out. Change the filter or use all sites with 'run --all'.",
        );
    }

    let repo = git::Repo::new();
    // `Repo::new` guarantees the repository top level is the working directory.
    // Inside a git repository the state lives in `.git` so it never shows up
    // in `git status` and a clean repository stays clean. Without a git
    // repository a hidden directory in the working directory is used.
    let state_dir = match &repo {
        Ok(_) => PathBuf::from(state_store::GIT_STATE_DIR),
        Err(_) => PathBuf::from(state_store::PLAIN_STATE_DIR),
    };
    let state_store = Arc::new(
        state_store::StateStore::load(&state_dir)
            .expect("failed to load the persistent request state"),
    );
    match &repo {
        Ok(repo) => {
            if repo.is_something_modified() {
                if do_commit {
                    logger::error_exit(
                        "The git repository is unclean. --commit can only be used in a clean repository.",
                    );
                }
                logger::warn("The git repository is unclean.");
            }
        }
        Err(err) => {
            if do_commit {
                logger::error_exit(&format!(
                    "Not a git repository. --commit only works in git repos: {err:#}"
                ));
            }
            logger::warn("Not a git repository. Will run but won't do git actions.");
        }
    }

    if sites_amount == sites_total {
        let paths = Site::get_all_file_paths(&sites);
        let removed = site_store::remove_gone(&paths)
            .expect("Should be able to cleanup the superfluous files");
        for file in removed {
            logger::warn(&format!("Remove superfluous {}", file.display()));
        }
        // State of sites removed from the configuration is gone as well.
        let state_keys: Vec<PathBuf> = paths.iter().map(|path| path.with_extension("")).collect();
        state_store.prune(&state_keys);
    }

    if sites_amount < sites_total {
        logger::info(&format!(
            "Your configuration file contains {sites_total} sites of which {sites_amount} are selected by your filter."
        ));
    }

    let mut groups: HashMap<String, Vec<Site>> = HashMap::new();
    for site in sites {
        let host = site.url.host_str().unwrap().to_owned();
        groups.entry(host).or_default().push(site);
    }

    let distinct_hosts = groups.len();
    println!("Begin stalking of {sites_amount} sites on {distinct_hosts} hosts...");
    if distinct_hosts < sites_amount {
        logger::info(&format!(
            "Some sites are on the same host. There is a wait time of {WAIT_BETWEEN_SAME_HOST:?} between each request to the same host in order to reduce load on the server.",
        ));
    }

    let mut rx = {
        let (tx, rx) = channel(10);
        #[expect(clippy::iter_over_hash_type)]
        for (_, sites) in groups {
            let from = from.clone();
            let tx = tx.clone();
            let state_store = Arc::clone(&state_store);
            tokio::spawn(async move {
                for (i, site) in sites.into_iter().enumerate() {
                    if i > 0 {
                        sleep(WAIT_BETWEEN_SAME_HOST).await;
                    }
                    let result = stalk_and_save_site(&from, &site, &state_store).await;
                    tx.send((site.url, result, site.options.ignore_error))
                        .await
                        .expect("failed to send stalking result");
                }
            });
        }
        rx
    };

    let mut urls_of_interest = Vec::new();
    let mut error_occurred = false;
    let mut amount_done: usize = 0;
    while let Some((url, result, ignore_error)) = rx.recv().await {
        amount_done += 1;
        match result {
            Ok((
                change_kind,
                http::ResponseMeta {
                    http_version,
                    ip_version,
                    took,
                    url,
                    validators: _,
                },
            )) => {
                eprintln!(
                    "{amount_done:4}/{sites_amount} {change_kind:11} {:5}ms {http_version:?} {ip_version} {url}",
                    took.as_millis(),
                );
                match change_kind {
                    ChangeKind::Init | ChangeKind::Changed => {
                        urls_of_interest.push(url);
                    }
                    ChangeKind::ContentSame => {}
                }
            }
            Err(err) => {
                let message = format!("{url} {err:#}");
                if ignore_error {
                    logger::warn(&message);
                } else {
                    logger::error(&message);
                    error_occurred = true;
                }
            }
        }
    }

    // Persist validators and final URLs for the conditional requests of the
    // next run. Failed sites never updated the store, so their previous
    // successful state and snapshot remain intact.
    state_store
        .persist()
        .expect("failed to persist the request state");

    let commit = repo
        .ok()
        .filter(git::Repo::is_something_modified)
        .and_then(|repo| {
            if do_commit {
                repo.add_all();
                let message = commit_message::commit_message(&urls_of_interest);
                let id = repo.commit(&message);
                Some(id)
            } else {
                logger::warn("No commit is created without the --commit flag.");
                None
            }
        });

    if !urls_of_interest.is_empty() {
        let message = notification::generate_text(
            commit,
            notification_commit_template.or_else(github::commit_prefix),
            urls_of_interest,
        );
        if let Err(err) = notifications.send_reqwest(&message).await {
            logger::error(&format!("notifier failed to send with Err: {err:#}"));
        }
    }

    if error_occurred {
        logger::notice_exit("All done but some site failed. Thank you for using website stalker!");
    }
    eprintln!("Thank you for using website-stalker!");
}

async fn stalk_and_save_site(
    from: &HeaderValue,
    site: &Site,
    state_store: &state_store::StateStore,
) -> anyhow::Result<(ChangeKind, http::ResponseMeta)> {
    let mut headers = site.options.headers.clone();
    if !headers.contains_key(FROM) {
        headers.insert(FROM, from.clone());
    }

    // Use site.url as the file basename should only change when the config
    // changes (manually). It is also the key for the persistent state.
    let mut path = site.to_file_path();
    let state_key = path.with_extension("");
    let previous = state_store.get(&state_key);

    // Request the final URL of the last run to avoid repeating redirects.
    let request_url = previous
        .as_ref()
        .map_or(site.url.as_str(), |state| state.url.as_str());
    let validators = previous.as_ref().map(|state| &state.validators);

    let outcome = http::get(
        request_url,
        headers,
        validators,
        site.options.accept_invalid_certs,
        site.options.http1_only,
    )
    .await
    .map_err(reqwest::Error::without_url)?;

    let (content, response) = match outcome {
        http::GetOutcome::NotModified(response) => {
            // The server says nothing changed. The stored snapshot remains
            // the source of truth and no content change is reported.
            if site_store::snapshot_exists(&path)? {
                return Ok((ChangeKind::ContentSame, response));
            }
            // No local snapshot to reuse: drop the stale state so the next
            // run does a plain GET again instead of another useless 304.
            state_store.remove(&state_key);
            anyhow::bail!(
                "server answered 304 Not Modified but no local snapshot exists for {}",
                site.url
            );
        }
        http::GetOutcome::Fresh(content, response) => (content, response),
    };

    if request_url != response.url.as_str() {
        logger::warn(&format!(
            "The URL {} was redirected to {}. This caused additional traffic which can be reduced by changing the URL to the target one.",
            request_url, response.url
        ));
    }

    // Use response.url as canonical urls for example are relative to the actual url
    let content = editor::Editor::apply_many(&site.options.editors, &response.url, content)?;
    let extension = content.extension.unwrap_or("txt");
    path.set_extension(extension);

    // Remember the validators and final URL only after a successful response.
    // On any failure above neither the state nor the stored snapshot changes.
    state_store.put(
        state_key,
        state_store::SiteState {
            url: response.url.clone(),
            validators: response.validators.clone(),
        },
    );

    let changed = site_store::write_only_changed(&path, &content.text)?;
    Ok((changed, response))
}
