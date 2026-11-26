//! Path patterns used by the capture policy.
//!
//! Patterns are written with `/` separators and matched against paths
//! relative to a source root:
//!
//! * `*` matches any run of characters inside one path component, `?` one
//!   character, `[a-z]` / `[!a-z]` a character class, `\x` the character `x`.
//! * `**` as a whole component matches zero or more components.
//! * A pattern without `/` matches a component by name at any depth.
//! * A pattern with `/` (a leading `/` counts) is anchored at the root.
//! * A trailing `/` is ignored.

use crate::error::CaptureError;

#[derive(Clone, Debug)]
enum Tok {
    Char(char),
    One,
    Star,
    Class {
        negated: bool,
        items: Vec<(char, char)>,
    },
}

impl Tok {
    fn accepts(&self, c: char) -> bool {
        match self {
            Tok::Char(expected) => *expected == c,
            Tok::One | Tok::Star => true,
            Tok::Class { negated, items } => {
                items.iter().any(|(lo, hi)| (*lo..=*hi).contains(&c)) != *negated
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Seg {
    /// `**`: zero or more whole components.
    Any,
    Literal(String),
    /// `text*`
    Prefix(String),
    /// `*text`
    Suffix(String),
    Toks(Vec<Tok>),
}

impl Seg {
    fn matches(&self, component: &str) -> bool {
        match self {
            Seg::Any => true,
            Seg::Literal(text) => text == component,
            Seg::Prefix(text) => component.starts_with(text.as_str()),
            Seg::Suffix(text) => component.ends_with(text.as_str()),
            Seg::Toks(toks) => toks_match(toks, component),
        }
    }
}

fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn fold(text: &str) -> String {
    text.chars().map(fold_char).collect()
}

/// Wildcard match with a single backtrack point: linear in practice and
/// never exponential.
fn toks_match(toks: &[Tok], text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let (mut t, mut c) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if t < toks.len() {
            if matches!(toks[t], Tok::Star) {
                star = Some((t, c));
                t += 1;
                continue;
            }
            if c < chars.len() && toks[t].accepts(chars[c]) {
                t += 1;
                c += 1;
                continue;
            }
        } else if c == chars.len() {
            return true;
        }
        match star {
            Some((at, consumed)) if consumed < chars.len() => {
                star = Some((at, consumed + 1));
                t = at + 1;
                c = consumed + 1;
            }
            _ => return false,
        }
    }
}

fn segs_match(segs: &[Seg], components: &[&str]) -> bool {
    let (mut s, mut c) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if s < segs.len() {
            if matches!(segs[s], Seg::Any) {
                star = Some((s, c));
                s += 1;
                continue;
            }
            if c < components.len() && segs[s].matches(components[c]) {
                s += 1;
                c += 1;
                continue;
            }
        } else if c == components.len() {
            return true;
        }
        match star {
            Some((at, consumed)) if consumed < components.len() => {
                star = Some((at, consumed + 1));
                s = at + 1;
                c = consumed + 1;
            }
            _ => return false,
        }
    }
}

fn parse_segment(text: &str, fold_case: bool) -> Result<Seg, String> {
    if text == "**" {
        return Ok(Seg::Any);
    }
    let lower = |c: char| if fold_case { fold_char(c) } else { c };
    let chars: Vec<char> = text.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 1;
                let c = chars.get(i).ok_or("ends with an unfinished `\\` escape")?;
                toks.push(Tok::Char(lower(*c)));
            }
            '*' => {
                if !matches!(toks.last(), Some(Tok::Star)) {
                    toks.push(Tok::Star);
                }
            }
            '?' => toks.push(Tok::One),
            '[' => {
                i += 1;
                let negated = matches!(chars.get(i), Some('!' | '^'));
                if negated {
                    i += 1;
                }
                let first = i;
                let mut items = Vec::new();
                loop {
                    let unclosed = "has an unclosed `[`";
                    let mut lo = *chars.get(i).ok_or(unclosed)?;
                    if lo == ']' && i > first {
                        break;
                    }
                    if lo == '\\' {
                        i += 1;
                        lo = *chars.get(i).ok_or(unclosed)?;
                    }
                    let mut hi = lo;
                    if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|c| *c != ']')
                    {
                        i += 2;
                        hi = chars[i];
                        if hi == '\\' {
                            i += 1;
                            hi = *chars.get(i).ok_or(unclosed)?;
                        }
                    }
                    let (lo, hi) = (lower(lo), lower(hi));
                    if lo > hi {
                        return Err(format!("has a reversed range `{lo}-{hi}`"));
                    }
                    items.push((lo, hi));
                    i += 1;
                }
                toks.push(Tok::Class { negated, items });
            }
            c => toks.push(Tok::Char(lower(c))),
        }
        i += 1;
    }

    let text_of = |toks: &[Tok]| -> Option<String> {
        toks.iter()
            .map(|t| match t {
                Tok::Char(c) => Some(*c),
                _ => None,
            })
            .collect()
    };
    if let Some(literal) = text_of(&toks) {
        return Ok(Seg::Literal(literal));
    }
    if let [head @ .., Tok::Star] = toks.as_slice()
        && let Some(prefix) = text_of(head)
    {
        return Ok(Seg::Prefix(prefix));
    }
    if let [Tok::Star, tail @ ..] = toks.as_slice()
        && let Some(suffix) = text_of(tail)
    {
        return Ok(Seg::Suffix(suffix));
    }
    Ok(Seg::Toks(toks))
}

/// One compiled pattern.
#[derive(Clone, Debug)]
pub(crate) struct Pattern {
    raw: String,
    anchored: bool,
    segs: Vec<Seg>,
}

impl Pattern {
    fn parse(raw: &str, fold_case: bool) -> Result<Self, String> {
        if raw.contains('\0') {
            return Err("contains NUL".into());
        }
        let mut text = raw;
        while let Some(rest) = text.strip_prefix("./") {
            text = rest;
        }
        let mut anchored = false;
        if let Some(rest) = text.strip_prefix('/') {
            anchored = true;
            text = rest;
        }
        let text = text.trim_end_matches('/');
        if text.is_empty() {
            return Err("names nothing below the root".into());
        }
        anchored |= text.contains('/');
        let mut segs = Vec::new();
        for component in text.split('/') {
            match component {
                "" => return Err("has an empty path component".into()),
                "." | ".." => return Err(format!("has a `{component}` component")),
                _ => segs.push(parse_segment(component, fold_case)?),
            }
        }
        Ok(Self {
            raw: raw.to_string(),
            anchored,
            segs,
        })
    }

    /// The pattern as the policy wrote it.
    pub(crate) fn raw(&self) -> &str {
        &self.raw
    }

    /// The path a wildcard-free pattern names, if it has no wildcards.
    pub(crate) fn literal_path(&self) -> Option<String> {
        let parts: Option<Vec<&str>> = self
            .segs
            .iter()
            .map(|seg| match seg {
                Seg::Literal(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        parts.map(|parts| parts.join("/"))
    }

    /// The pattern, compared without case folding, matches `rel` or one of
    /// its parent directories.
    pub(crate) fn covers(&self, rel: &str) -> bool {
        let components: Vec<&str> = rel.split('/').collect();
        self.matches_tree(&components)
    }

    /// The pattern matches the path itself: by full path when anchored, by
    /// file name otherwise.
    fn matches_file(&self, components: &[&str]) -> bool {
        if self.anchored {
            segs_match(&self.segs, components)
        } else {
            components
                .last()
                .is_some_and(|name| self.segs[0].matches(name))
        }
    }

    /// The pattern matches the path or one of its parent directories.
    fn matches_tree(&self, components: &[&str]) -> bool {
        if self.anchored {
            (1..=components.len()).any(|n| segs_match(&self.segs, &components[..n]))
        } else {
            components.iter().any(|name| self.segs[0].matches(name))
        }
    }
}

/// One of the policy's pattern lists.
#[derive(Clone, Debug)]
pub(crate) struct PatternSet {
    patterns: Vec<Pattern>,
    fold_case: bool,
}

impl PatternSet {
    pub(crate) fn parse(
        list: &'static str,
        raw: &[String],
        fold_case: bool,
    ) -> Result<Self, CaptureError> {
        let patterns = raw
            .iter()
            .map(|pattern| {
                Pattern::parse(pattern, fold_case).map_err(|reason| CaptureError::InvalidPattern {
                    list,
                    pattern: pattern.clone(),
                    reason,
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            patterns,
            fold_case,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub(crate) fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }

    fn find(&self, rel: &str, test: impl Fn(&Pattern, &[&str]) -> bool) -> Option<&Pattern> {
        if self.patterns.is_empty() {
            return None;
        }
        let folded;
        let rel = if self.fold_case {
            folded = fold(rel);
            folded.as_str()
        } else {
            rel
        };
        let components: Vec<&str> = rel.split('/').collect();
        self.patterns.iter().find(|p| test(p, &components))
    }

    /// First pattern that matches the file by name or by full relative path.
    pub(crate) fn file_match(&self, rel: &str) -> Option<&Pattern> {
        self.find(rel, Pattern::matches_file)
    }

    /// First pattern that matches the path or any directory above it.
    pub(crate) fn tree_match(&self, rel: &str) -> Option<&Pattern> {
        self.find(rel, Pattern::matches_tree)
    }

    /// A wildcard-free pattern names exactly this file.
    pub(crate) fn names_file(&self, rel: &str) -> bool {
        self.find(rel, |p, components| {
            p.literal_path().is_some() && p.matches_file(components)
        })
        .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(patterns: &[&str]) -> PatternSet {
        let raw: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        PatternSet::parse("test", &raw, false).unwrap()
    }

    fn file(pattern: &str, path: &str) -> bool {
        set(&[pattern]).file_match(path).is_some()
    }

    fn tree(pattern: &str, path: &str) -> bool {
        set(&[pattern]).tree_match(path).is_some()
    }

    #[test]
    fn unanchored_patterns_match_names_at_any_depth() {
        assert!(file(".env", ".env"));
        assert!(file(".env", "services/api/.env"));
        assert!(!file(".env", ".env/readme"));
        assert!(file(".env.*", "a/.env.local"));
        assert!(!file(".env.*", "a/.env"));
        assert!(file("*.pem", "certs/server.pem"));
        assert!(!file("*.pem", "certs/server.pem.txt"));
        assert!(file("id_rsa*", "keys/id_rsa.pub"));
        assert!(file("*", "anything"));
    }

    #[test]
    fn anchored_patterns_match_the_relative_path() {
        assert!(file("config/*.key", "config/prod.key"));
        assert!(!file("config/*.key", "other/config/prod.key"));
        assert!(!file("config/*.key", "config/sub/prod.key"));
        assert!(file("/top.txt", "top.txt"));
        assert!(!file("/top.txt", "sub/top.txt"));
        assert!(file("**/prod.key", "prod.key"));
        assert!(file("**/prod.key", "a/b/prod.key"));
        assert!(file("a/**/z", "a/z"));
        assert!(file("a/**/z", "a/b/c/z"));
        assert!(!file("a/**/z", "a/b/c"));
        assert!(file("a/**", "a/b/c"));
        assert!(!file("a/**/b/**/c", "a/b/x"));
        assert!(file("a/**/b/**/c", "a/x/b/y/b/c"));
    }

    #[test]
    fn tree_matching_covers_everything_under_a_directory() {
        assert!(tree("node_modules", "web/node_modules/pkg/index.js"));
        assert!(tree("build/out", "build/out/gen/a.h"));
        assert!(!tree("build/out", "other/build/out/a.h"));
        assert!(tree("build/", "build/a"));
        assert!(!tree("*.log", "logs/app.txt"));
        assert!(tree("*.log", "logs/app.log"));
    }

    #[test]
    fn wildcards_classes_and_escapes() {
        assert!(file("a?c", "abc"));
        assert!(!file("a?c", "ac"));
        assert!(file("*a*b*c*", "xxaxxbxxcxx"));
        assert!(!file("*a*b*c*", "xxaxxcxxbxx"));
        assert!(file("[a-c]x", "bx"));
        assert!(!file("[!a-c]x", "bx"));
        assert!(file("[]]x", "]x"));
        assert!(file(r"\*.txt", "*.txt"));
        assert!(!file(r"\*.txt", "a.txt"));
        assert!(file("ünï*", "ünï côdé"));
        // `*` never crosses a separator.
        assert!(!file("a/*", "a/b/c"));
    }

    #[test]
    fn case_folding_is_opt_in() {
        let raw = vec![".env".to_string(), "*.PEM".to_string()];
        let folded = PatternSet::parse("secrets", &raw, true).unwrap();
        assert!(folded.file_match("a/.ENV").is_some());
        assert!(folded.file_match("Key.pem").is_some());
        let exact = PatternSet::parse("secrets", &raw, false).unwrap();
        assert!(exact.file_match("a/.ENV").is_none());
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        for bad in [
            "",
            "/",
            "a//b",
            "a/../b",
            "[abc",
            "trailing\\",
            "[z-a]",
            "./",
        ] {
            assert!(
                PatternSet::parse("exclude", &[bad.to_string()], false).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn literal_patterns_are_recognised() {
        let s = set(&["gen/out.h", "gen/*.h", r"odd\*name"]);
        let literals: Vec<Option<String>> =
            s.patterns().iter().map(Pattern::literal_path).collect();
        assert_eq!(
            literals,
            [Some("gen/out.h".to_string()), None, Some("odd*name".into())]
        );
        assert!(s.names_file("gen/out.h"));
        assert!(!s.names_file("gen/other.h"));
    }
}
