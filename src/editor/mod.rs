use std::path::PathBuf;

use anyhow::Context as _;
use schemars::JsonSchema;
use serde::Deserialize;
use url::Url;

pub mod css_flatten;
pub mod css_remove;
pub mod css_selector;
pub mod css_sort;
pub mod css_tag_replace;
pub mod debug_files;
pub mod html_markdown;
pub mod html_pretty;
pub mod html_sanitize;
pub mod html_text;
pub mod html_url;
pub mod json_prettify;
pub mod json_simple_select;
pub mod regex_replacer;
pub mod rss;

/// Content of a response while it passes the editor pipeline.
///
/// Responses are fetched as raw bytes and stay untouched unless an editor is
/// configured. As soon as an editor runs, text-like content is decoded into a
/// [`String`] and all following editors work on that decoded text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// Raw response bytes. Used when no editor is configured so that binary
    /// content like PDFs or images is stored byte for byte.
    Bytes(Vec<u8>),

    /// Decoded text. Produced from `Bytes` when the first editor runs or
    /// directly by an editor which only works on text.
    Text(String),
}

impl Content {
    pub const fn text(text: String) -> Self {
        Self::Text(text)
    }

    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::Text(text) => text.as_bytes(),
        }
    }

    /// Decode the content as text.
    ///
    /// Raw bytes are decoded with the given charset which has to be determined
    /// from the `Content-Type` header (and byte sniffing as a fallback) before
    /// calling this. Already decoded text is returned unchanged.
    ///
    /// Like a browser (and the previous `response.text()` behaviour) malformed
    /// byte sequences are replaced with the Unicode replacement character
    /// instead of failing so a single bad byte does not break the whole run.
    /// An unknown charset label is still an error as there is no sensible
    /// decoder to fall back to.
    pub fn into_text(self, charset: &str) -> anyhow::Result<Self> {
        match self {
            Self::Text(_) => Ok(self),
            Self::Bytes(bytes) => {
                let encoding = encoding_rs::Encoding::for_label(charset.as_bytes())
                    .with_context(|| format!("unknown charset '{charset}'"))?;
                let (text, _encoding, _had_errors) = encoding.decode(&bytes);
                Ok(Self::Text(text.into_owned()))
            }
        }
    }

    /// Return the decoded text. Only valid while the content is `Text`, which
    /// is always the case inside the editor pipeline after `into_text`.
    pub fn as_text(&self) -> &str {
        match self {
            Self::Text(text) => text,
            Self::Bytes(_) => unreachable!("editors only receive decoded text"),
        }
    }
}

/// A response after running editors on it.
///
/// The extension describes the format of the content so the caller can pick
/// the output filename. `None` keeps the extension derived from the response.
#[derive(Debug)]
pub struct Processed {
    pub content: Content,
    pub extension: Option<String>,
}

/// # Editor
/// Editors are manipulating the content of a webpage to simplify comparing them later on.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum Editor {
    CssFlatten(#[schemars(with = "String")] scraper::Selector),
    CssRemove(#[schemars(with = "String")] scraper::Selector),
    CssSelect(#[schemars(with = "String")] scraper::Selector),
    CssSort(css_sort::CssSort),
    CssTagReplace(css_tag_replace::CssTagReplace),
    DebugFiles(PathBuf),
    HtmlMarkdownify,
    HtmlPrettify,
    HtmlSanitize,
    HtmlTextify,
    HtmlUrlCanonicalize,
    JsonPrettify,
    JsonSimpleSelect(String),
    RegexReplace(regex_replacer::RegexReplacer),
    Rss(rss::Rss),
}

impl Editor {
    pub const fn log_name(&self) -> &'static str {
        match self {
            Self::CssFlatten(_) => "css_flatten",
            Self::CssRemove(_) => "css_remove",
            Self::CssSelect(_) => "css_select",
            Self::CssSort(_) => "css_sort",
            Self::CssTagReplace(_) => "css_tag_replace",
            Self::DebugFiles(_) => "debug_files",
            Self::HtmlMarkdownify => "html_markdownify",
            Self::HtmlPrettify => "html_prettify",
            Self::HtmlSanitize => "html_sanitize",
            Self::HtmlTextify => "html_textify",
            Self::HtmlUrlCanonicalize => "html_url_canonicalize",
            Self::JsonPrettify => "json_prettify",
            Self::JsonSimpleSelect(_) => "json_simple_select",
            Self::RegexReplace(_) => "regex_replace",
            Self::Rss(_) => "rss",
        }
    }

    /// Extension of the editor output, or `None` when the input extension is
    /// kept (`regex_replace`, `debug_files`).
    const fn output_extension(&self) -> Option<&'static str> {
        match self {
            Self::HtmlMarkdownify => Some("md"),
            Self::HtmlTextify => Some("txt"),
            Self::JsonPrettify | Self::JsonSimpleSelect(_) => Some("json"),
            Self::Rss(_) => Some("xml"),
            Self::CssFlatten(_)
            | Self::CssRemove(_)
            | Self::CssSelect(_)
            | Self::CssSort(_)
            | Self::CssTagReplace(_)
            | Self::HtmlPrettify
            | Self::HtmlSanitize
            | Self::HtmlUrlCanonicalize => Some("html"),
            Self::DebugFiles(_) | Self::RegexReplace(_) => None,
        }
    }

    fn apply(&self, url: &Url, input: Content) -> anyhow::Result<Content> {
        let text = input.as_text();
        let content = match &self {
            Self::CssFlatten(selector) => Content::text(css_flatten::apply(selector, text)),
            Self::CssRemove(selector) => Content::text(css_remove::apply(selector, text)),
            Self::CssSelect(selector) => Content::text(css_selector::apply(selector, text)?),
            Self::CssSort(sort) => Content::text(sort.apply(url, text)),
            Self::CssTagReplace(replace) => Content::text(replace.apply(text)?),
            Self::DebugFiles(path) => return debug_files::debug_files(path, input),
            Self::HtmlMarkdownify => Content::text(html_markdown::markdownify(text)?),
            Self::HtmlPrettify => Content::text(html_pretty::prettify(text)?),
            Self::HtmlSanitize => Content::text(html_sanitize::sanitize(text)),
            Self::HtmlTextify => Content::text(html_text::textify(text)?),
            Self::HtmlUrlCanonicalize => Content::text(html_url::canonicalize(url, text)?),
            Self::JsonPrettify => Content::text(json_prettify::prettify(text)?),
            Self::JsonSimpleSelect(selector) => {
                Content::text(json_simple_select::apply(text, selector)?)
            }
            Self::RegexReplace(rr) => Content::text(rr.replace_all(text).to_string()),
            Self::Rss(rss) => Content::text(rss.generate(url, text)?),
        };
        Ok(content)
    }

    /// Run all editors in order on the response content.
    ///
    /// When at least one editor is configured the raw bytes are decoded once
    /// using `charset`. Without editors the bytes pass through untouched so
    /// binary responses stay byte for byte identical.
    pub fn apply_many(
        editors: &[Self],
        url: &Url,
        content: Content,
        charset: &str,
        mut extension: Option<String>,
    ) -> anyhow::Result<Processed> {
        let mut content = content;
        if !editors.is_empty() {
            content = content
                .into_text(charset)
                .context("failed to decode the response for editing")?;
        }
        for (i, editor) in editors.iter().enumerate() {
            content = editor
                .apply(url, content)
                .with_context(|| format!("in editor[{i}] {}", editor.log_name()))?;
            if let Some(output_extension) = editor.output_extension() {
                extension = Some(output_extension.to_owned());
            }
        }
        Ok(Processed { content, extension })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_pass_through_without_editors() {
        let input = Content::Bytes(vec![0x89, b'P', b'N', b'G', 0x00, 0xFF]);
        let url = Url::parse("https://example.com/image.png").unwrap();
        let processed =
            Editor::apply_many(&[], &url, input.clone(), "utf-8", Some("png".to_owned())).unwrap();
        assert_eq!(processed.content, input);
        assert_eq!(processed.extension.as_deref(), Some("png"));
    }

    #[test]
    fn editors_decode_and_change_extension() {
        let input = Content::Bytes(b"<html><body>Hi</body></html>".to_vec());
        let url = Url::parse("https://example.com/").unwrap();
        let processed = Editor::apply_many(
            &[Editor::HtmlTextify],
            &url,
            input,
            "utf-8",
            Some("html".to_owned()),
        )
        .unwrap();
        assert_eq!(processed.content.as_text(), "Hi");
        assert_eq!(processed.extension.as_deref(), Some("txt"));
    }

    #[test]
    fn decodes_utf16le_with_bom() {
        // BOM + "Hi" encoded as UTF-16LE. encoding_rs consumes the BOM.
        let bytes = vec![0xFF, 0xFE, b'H', 0x00, b'i', 0x00];
        let content = Content::Bytes(bytes).into_text("utf-16le").unwrap();
        assert_eq!(content.as_text(), "Hi");
    }

    #[test]
    fn decodes_utf16be_with_bom() {
        // BOM + "Hi" encoded as UTF-16BE. encoding_rs consumes the BOM.
        let bytes = vec![0xFE, 0xFF, 0x00, b'H', 0x00, b'i'];
        let content = Content::Bytes(bytes).into_text("utf-16be").unwrap();
        assert_eq!(content.as_text(), "Hi");
    }

    #[test]
    fn decodes_utf8() {
        let bytes = "Grüße".as_bytes().to_vec();
        let content = Content::Bytes(bytes).into_text("utf-8").unwrap();
        assert_eq!(content.as_text(), "Grüße");
    }

    #[test]
    fn invalid_bytes_become_replacement_characters() {
        // Matches the previous response.text() behaviour: never fail on bad
        // bytes, substitute the Unicode replacement character instead.
        let content = Content::Bytes(vec![b'a', 0xFF, 0xFD, b'b'])
            .into_text("utf-8")
            .unwrap();
        assert_eq!(content.as_text(), "a\u{FFFD}\u{FFFD}b");
    }

    #[test]
    fn unknown_charset_is_an_error() {
        let content = Content::Bytes(b"hello".to_vec()).into_text("not-a-real-charset");
        assert!(content.is_err());
    }

    #[test]
    fn text_is_not_decoded_again() {
        let content = Content::Text("already text".to_owned())
            .into_text("utf-16le")
            .unwrap();
        assert_eq!(content.as_text(), "already text");
    }

    #[test]
    fn regex_replace_keeps_the_extension() {
        let input = Content::Text("a1b2".to_owned());
        let url = Url::parse("https://example.com/").unwrap();
        let editor = Editor::RegexReplace(regex_replacer::RegexReplacer {
            pattern: regex::Regex::new(r"\d").unwrap(),
            replace: "x".to_owned(),
        });
        let processed =
            Editor::apply_many(&[editor], &url, input, "utf-8", Some("txt".to_owned())).unwrap();
        assert_eq!(processed.content.as_text(), "axbx");
        assert_eq!(processed.extension.as_deref(), Some("txt"));
    }
}
