use core::time::Duration;
use std::net::SocketAddr;
use std::time::Instant;

use anyhow::Context as _;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH};
use reqwest::{ClientBuilder, header};
use url::Url;

use crate::editor::Content;
use crate::state_store::Validators;

const USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " ",
    env!("CARGO_PKG_REPOSITORY"),
);

pub struct ResponseMeta {
    pub http_version: reqwest::Version,
    pub ip_version: IpVersion,
    pub took: Duration,
    /// Get the final `Url` of this `Response`.
    pub url: Url,
    /// Validators returned by the server for the next conditional request.
    pub validators: Validators,
}

#[derive(Debug)]
pub enum IpVersion {
    IPv4,
    IPv6,
    None,
}

impl core::fmt::Display for IpVersion {
    fn fmt(&self, fmt: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self, fmt)
    }
}

/// Outcome of a GET request.
pub enum GetOutcome {
    /// `200 OK` with the response body.
    Fresh(Content, ResponseMeta),
    /// `304 Not Modified`: the server confirmed the content did not change.
    /// There is no response body and the stored snapshot stays valid.
    NotModified(ResponseMeta),
}

/// HTTP GET Request
///
/// FROM provides an email address for the target host to be contacted in case of problems.
/// See [HTTP From header](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/From)
///
/// When `validators` are available they are sent as conditional request
/// headers (`If-None-Match` / `If-Modified-Since`).
/// A `304 Not Modified` response is returned as [`GetOutcome::NotModified`]
/// without downloading the body again.
pub async fn get(
    url: &str,
    additional_headers: HeaderMap,
    validators: Option<&Validators>,
    accept_invalid_certs: bool,
    http1_only: bool,
) -> reqwest::Result<GetOutcome> {
    let mut builder = ClientBuilder::new()
        .danger_accept_invalid_certs(accept_invalid_certs)
        .timeout(Duration::from_secs(30))
        .user_agent(HeaderValue::from_static(USER_AGENT));
    if http1_only {
        builder = builder.http1_only();
    }
    let mut request = builder.build()?.get(url).headers(additional_headers);
    if let Some(validators) = validators {
        if let Some(etag) = &validators.etag
            && let Ok(value) = HeaderValue::from_str(etag)
        {
            request = request.header(IF_NONE_MATCH, value);
        }
        if let Some(last_modified) = &validators.last_modified
            && let Ok(value) = HeaderValue::from_str(last_modified)
        {
            request = request.header(IF_MODIFIED_SINCE, value);
        }
    }

    let start = Instant::now();
    let response = request.send().await?;
    let not_modified = response.status() == reqwest::StatusCode::NOT_MODIFIED;
    let response = response.error_for_status()?;
    let took = Instant::now().saturating_duration_since(start);

    let mut meta = build_meta(&response, took);
    if not_modified {
        // A 304 usually updates the validators as well. Keep the previously
        // known ones when the 304 does not provide a header.
        if let Some(validators) = validators {
            if meta.validators.etag.is_none() {
                meta.validators.etag.clone_from(&validators.etag);
            }
            if meta.validators.last_modified.is_none() {
                meta.validators
                    .last_modified
                    .clone_from(&validators.last_modified);
            }
        }
        return Ok(GetOutcome::NotModified(meta));
    }

    let extension = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(mime2ext::mime2ext);
    let text = response.text().await?;
    let content = Content { extension, text };
    Ok(GetOutcome::Fresh(content, meta))
}

fn build_meta(response: &reqwest::Response, took: Duration) -> ResponseMeta {
    let headers = response.headers();
    let ip_version = match response.remote_addr() {
        Some(SocketAddr::V4(_)) => IpVersion::IPv4,
        Some(SocketAddr::V6(_)) => IpVersion::IPv6,
        None => IpVersion::None,
    };
    let validators = Validators {
        etag: header_value(headers, header::ETAG),
        last_modified: header_value(headers, header::LAST_MODIFIED),
    };
    ResponseMeta {
        http_version: response.version(),
        ip_version,
        took,
        url: response.url().clone(),
        validators,
    }
}

fn header_value(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

pub fn validate_from(from: &str) -> anyhow::Result<()> {
    let value = HeaderValue::from_str(from)?;
    let value = value.to_str().context("contains non ASCII characters")?;
    if !value.contains('@') || !value.contains('.') {
        anyhow::bail!("doesnt look like an email address: {from}");
    }

    Ok(())
}

#[test]
fn from_is_email() {
    validate_from("foo@bar.de").unwrap();
}

#[test]
#[should_panic = "doesnt look like an email address"]
fn from_is_no_email() {
    validate_from("bla.de").unwrap();
}

#[test]
#[should_panic = "ASCII char"]
fn from_is_no_ascii() {
    validate_from("föo@bär.de").unwrap();
}
