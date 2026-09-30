//! `include` / `exclude` URL patterns for `enqueue_links`.

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;

/// A URL pattern: a glob (matched case-insensitively, `*` stays within one path segment, `**` spans
/// segments, `{a,b}` alternatives) or a regular expression.
///
/// Known differences from Crawlee for JS (listed in `allowed-differences.md`):
/// - globs are matched by `globset`, not `minimatch`; `*` also matches a leading `.` in a segment,
/// - regular expressions use the `regex` crate syntax (no look-around, no backreferences).
#[derive(Clone, Debug)]
pub enum UrlPattern {
    Glob(String),
    Regex(Regex),
}

impl UrlPattern {
    pub fn glob(pattern: impl Into<String>) -> Self {
        UrlPattern::Glob(pattern.into())
    }

    pub fn regex(pattern: &str) -> Result<Self, regex::Error> {
        Ok(UrlPattern::Regex(Regex::new(pattern)?))
    }
}

impl From<&str> for UrlPattern {
    fn from(value: &str) -> Self {
        UrlPattern::glob(value)
    }
}

impl From<String> for UrlPattern {
    fn from(value: String) -> Self {
        UrlPattern::glob(value)
    }
}

impl From<Regex> for UrlPattern {
    fn from(value: Regex) -> Self {
        UrlPattern::Regex(value)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PatternError {
    #[error("Cannot parse Glob pattern '{0}': it must be a non-empty string")]
    EmptyGlob(String),
    #[error("Cannot parse Glob pattern '{pattern}': {source}")]
    InvalidGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },
}

/// A compiled [`UrlPattern`].
#[derive(Clone, Debug)]
pub enum UrlMatcher {
    Glob(GlobMatcher),
    Regex(Regex),
}

impl UrlMatcher {
    pub fn compile(pattern: &UrlPattern) -> Result<Self, PatternError> {
        match pattern {
            UrlPattern::Glob(glob) => {
                let trimmed = glob.trim();
                if trimmed.is_empty() {
                    return Err(PatternError::EmptyGlob(glob.clone()));
                }
                let matcher = GlobBuilder::new(trimmed)
                    .case_insensitive(true)
                    .literal_separator(true)
                    .empty_alternates(true)
                    .build()
                    .map_err(|source| PatternError::InvalidGlob { pattern: trimmed.to_owned(), source })?
                    .compile_matcher();
                Ok(UrlMatcher::Glob(matcher))
            }
            UrlPattern::Regex(regex) => Ok(UrlMatcher::Regex(regex.clone())),
        }
    }

    pub fn is_match(&self, url: &str) -> bool {
        match self {
            UrlMatcher::Glob(glob) => glob.is_match(url),
            UrlMatcher::Regex(regex) => regex.is_match(url),
        }
    }
}

/// Compiled `include` and `exclude` lists. A URL passes when it matches no `exclude` pattern and,
/// if any `include` patterns are given, at least one of them.
#[derive(Clone, Debug, Default)]
pub struct UrlFilter {
    include: Vec<UrlMatcher>,
    exclude: Vec<UrlMatcher>,
}

impl UrlFilter {
    pub fn new(include: &[UrlPattern], exclude: &[UrlPattern]) -> Result<Self, PatternError> {
        Ok(UrlFilter {
            include: include.iter().map(UrlMatcher::compile).collect::<Result<_, _>>()?,
            exclude: exclude.iter().map(UrlMatcher::compile).collect::<Result<_, _>>()?,
        })
    }

    pub fn is_allowed(&self, url: &str) -> bool {
        if self.exclude.iter().any(|m| m.is_match(url)) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(|m| m.is_match(url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_behave_like_minimatch() {
        let filter = UrlFilter::new(&["http{s,}://example.com/**".into()], &["**/*.pdf".into()]).unwrap();
        assert!(filter.is_allowed("https://example.com/a/b"));
        assert!(filter.is_allowed("http://EXAMPLE.com/a"));
        assert!(!filter.is_allowed("https://example.com/a/file.pdf"));
        assert!(!filter.is_allowed("https://other.com/a"));

        let single = UrlFilter::new(&["https://example.com/*".into()], &[]).unwrap();
        assert!(single.is_allowed("https://example.com/a"));
        assert!(!single.is_allowed("https://example.com/a/b"));
    }

    #[test]
    fn regex_patterns() {
        let filter = UrlFilter::new(&[UrlPattern::regex(r"/product/\d+$").unwrap()], &[]).unwrap();
        assert!(filter.is_allowed("https://shop.dev/product/42"));
        assert!(!filter.is_allowed("https://shop.dev/product/abc"));
    }

    #[test]
    fn empty_glob_is_rejected() {
        assert!(UrlFilter::new(&["  ".into()], &[]).is_err());
    }
}
