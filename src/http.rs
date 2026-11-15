use core::time::Duration;
use std::net::SocketAddr;
use std::time::Instant;

use anyhow::Context as _;
use reqwest::ClientBuilder;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use url::Url;

use crate::editor::Content;

const USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " ",
    env!("CARGO_PKG_REPOSITORY"),
);

/// Response content which is relevant beyond the plain body bytes.
pub struct ResponseContent {
    pub content: Content,
    /// Extension derived from the `Content-Type` header or the final URL.
    pub extension: Option<String>,
    /// Charset label used to decode the body when editors are configured.
    pub charset: &'static str,
    /// Whether the body is text and therefore safe to decode for editors.
    pub is_text: bool,
}

pub struct ResponseMeta {
    pub http_version: reqwest::Version,
    pub ip_version: IpVersion,
    pub took: Duration,
    /// Get the final `Url` of this `Response`.
    pub url: Url,
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

/// HTTP GET Request
///
/// FROM provides an email address for the target host to be contacted in case of problems.
/// See [HTTP From header](https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/From)
pub async fn get(
    url: &str,
    additional_headers: HeaderMap,
    accept_invalid_certs: bool,
    http1_only: bool,
) -> reqwest::Result<(ResponseContent, ResponseMeta)> {
    let mut builder = ClientBuilder::new()
        .danger_accept_invalid_certs(accept_invalid_certs)
        .timeout(Duration::from_secs(30))
        .user_agent(HeaderValue::from_static(USER_AGENT));
    if http1_only {
        builder = builder.http1_only();
    }
    let request = builder.build()?.get(url).headers(additional_headers);

    let start = Instant::now();
    let response = request.send().await?.error_for_status()?;
    let took = Instant::now().saturating_duration_since(start);

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mime = content_type.as_deref().map(MimeType::parse);
    let ip_version = match response.remote_addr() {
        Some(SocketAddr::V4(_)) => IpVersion::IPv4,
        Some(SocketAddr::V6(_)) => IpVersion::IPv6,
        None => IpVersion::None,
    };
    let meta = ResponseMeta {
        http_version: response.version(),
        ip_version,
        took,
        url: response.url().clone(),
    };

    let bytes = response.bytes().await?;
    // Prefer the extension from Content-Type and fall back to the final URL.
    let extension = mime.as_ref().map_or_else(
        || extension_from_url(&meta.url),
        |mime| mime.extension().or_else(|| extension_from_url(&meta.url)),
    );
    let charset = mime
        .as_ref()
        .and_then(MimeType::charset)
        .unwrap_or_else(|| charset_from_bytes(&bytes));
    // Without a Content-Type the type is unknown. Assume text like browsers do
    // so editors keep working; an explicit binary type blocks editors instead.
    let is_text = mime.as_ref().is_none_or(MimeType::is_text);
    let content = ResponseContent {
        content: Content::Bytes(bytes.to_vec()),
        extension,
        charset,
        is_text,
    };
    Ok((content, meta))
}

/// Parsed parts of a `Content-Type` header.
struct MimeType<'a> {
    essence: &'a str,
    charset: Option<&'a str>,
}

impl<'a> MimeType<'a> {
    fn parse(header: &'a str) -> Self {
        let mut parts = header.split(';');
        let essence = parts.next().unwrap_or_default().trim();
        let charset = parts.find_map(|parameter| {
            let (key, value) = parameter.split_once('=')?;
            (key.trim().eq_ignore_ascii_case("charset")).then(|| value.trim().trim_matches('"'))
        });
        Self { essence, charset }
    }

    fn charset(&self) -> Option<&'static str> {
        let label = self.charset?;
        // Validate and canonicalize the label via encoding_rs so that unknown
        // or misspelled charsets fall back to sniffing instead of erroring.
        encoding_rs::Encoding::for_label(label.as_bytes()).map(encoding_rs::Encoding::name)
    }

    fn extension(&self) -> Option<String> {
        mime2ext::mime2ext(self.essence).map(str::to_owned)
    }

    fn is_text(&self) -> bool {
        let main_type = self
            .essence
            .split_once('/')
            .map_or(self.essence, |(main, _)| main);
        main_type.eq_ignore_ascii_case("text")
            || matches!(
                self.essence.to_ascii_lowercase().as_str(),
                "application/json"
                    | "application/xml"
                    | "application/javascript"
                    | "application/ecmascript"
                    | "application/x-javascript"
                    | "application/x-www-form-urlencoded"
                    | "application/xhtml+xml"
                    | "application/rss+xml"
                    | "application/atom+xml"
                    | "application/ld+json"
                    | "image/svg+xml"
            )
    }
}

/// Guess an extension from the path of the final URL after redirects.
fn extension_from_url(url: &Url) -> Option<String> {
    let path = url.path();
    let extension = path.rsplit_once('.')?.1;
    // The path segment has to be a real filename segment, not something like
    // `v1.2` or an empty string. Keep it conservative to avoid odd extensions.
    if extension.is_empty() || extension.len() > 8 || extension.contains('/') {
        return None;
    }
    extension
        .chars()
        .all(|character| character.is_ascii_alphanumeric())
        .then(|| extension.to_owned())
}

/// Determine the charset without a `Content-Type` charset parameter.
///
/// A byte order mark reliably identifies the endianness of UTF-16 content.
/// Without one UTF-8 is assumed as it is the dominant encoding on the web. A
/// leading UTF-8 BOM needs no special handling as the decoder strips it.
fn charset_from_bytes(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        encoding_rs::UTF_16LE.name()
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        encoding_rs::UTF_16BE.name()
    } else {
        encoding_rs::UTF_8.name()
    }
}

pub fn validate_from(from: &str) -> anyhow::Result<()> {
    let value = HeaderValue::from_str(from)?;
    let value = value.to_str().context("contains non ASCII characters")?;
    if !value.contains('@') || !value.contains('.') {
        anyhow::bail!("doesnt look like an email address: {from}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_parses_essence_and_charset() {
        let mime = MimeType::parse("text/html; charset=UTF-8");
        assert_eq!(mime.essence, "text/html");
        assert_eq!(mime.charset(), Some("UTF-8"));
        assert!(mime.is_text());
        assert_eq!(mime.extension().as_deref(), Some("html"));
    }

    #[test]
    fn mime_handles_quoted_and_uppercase_charset() {
        let mime = MimeType::parse("text/html; CHARSET=\"utf-16le\"");
        assert_eq!(mime.charset(), Some("UTF-16LE"));
        assert!(mime.is_text());
    }

    #[test]
    fn mime_unknown_charset_is_none() {
        let mime = MimeType::parse("text/html; charset=bogus");
        assert_eq!(mime.charset(), None);
    }

    #[test]
    fn mime_detects_text_and_binary() {
        assert!(MimeType::parse("text/plain").is_text());
        assert!(MimeType::parse("application/json").is_text());
        assert!(MimeType::parse("application/rss+xml; charset=utf-8").is_text());
        assert!(MimeType::parse("image/svg+xml").is_text());
        assert!(!MimeType::parse("application/pdf").is_text());
        assert!(!MimeType::parse("image/png").is_text());
        assert!(!MimeType::parse("application/octet-stream").is_text());
    }

    #[test]
    fn url_extension_is_detected() {
        let url = Url::parse("https://example.com/folder/document.pdf").unwrap();
        assert_eq!(extension_from_url(&url).as_deref(), Some("pdf"));
    }

    #[test]
    fn url_without_extension_returns_none() {
        let url = Url::parse("https://example.com/folder/").unwrap();
        assert_eq!(extension_from_url(&url), None);
    }

    #[test]
    fn utf16_bom_is_sniffed() {
        assert_eq!(charset_from_bytes(&[0xFF, 0xFE, 0x68, 0x00]), "UTF-16LE");
        assert_eq!(charset_from_bytes(&[0xFE, 0xFF, 0x00, 0x68]), "UTF-16BE");
    }

    #[test]
    fn utf8_is_the_default_charset() {
        assert_eq!(charset_from_bytes(b"just some text"), "UTF-8");
        assert_eq!(charset_from_bytes(&[0xEF, 0xBB, 0xBF, b'a']), "UTF-8");
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
}
