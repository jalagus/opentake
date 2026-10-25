//! Lossless-ish generic tree for Reaper's `.rpp` text format.
//!
//! An RPP file is a tree of chunks:
//!
//! ```text
//! <REAPER_PROJECT 0.1 "7.0/macOS-arm64" 1700000000
//!   TEMPO 120 4 4
//!   <TRACK {GUID}
//!     NAME "Guitar"
//!   >
//! >
//! ```
//!
//! Lines inside a chunk keep their original text, so anything we don't understand
//! (plugin state blobs, notes, envelopes, ...) is written back verbatim.

use std::fmt::Write as _;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("line {0}: unexpected '>' without an open chunk")]
    UnbalancedClose(usize),
    #[error("unexpected end of file: {0} chunk(s) left open")]
    UnclosedChunk(usize),
    #[error("file contains no root chunk")]
    Empty,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub name: String,
    pub params: Vec<String>,
    pub children: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Line(Line),
    Chunk(Node),
}

/// A single non-chunk line. `raw` is the original text (without indentation).
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub raw: String,
}

impl Line {
    pub fn from_tokens<S: AsRef<str>>(tokens: &[S]) -> Self {
        Self { raw: join_tokens(tokens) }
    }

    pub fn tokens(&self) -> Vec<String> {
        tokenize(&self.raw)
    }

    /// First token of the line, e.g. `"NAME"` for `NAME "Guitar"`.
    pub fn key(&self) -> &str {
        self.raw.split_whitespace().next().unwrap_or("")
    }
}

impl Node {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), params: Vec::new(), children: Vec::new() }
    }

    pub fn with_params<S: AsRef<str>>(mut self, params: &[S]) -> Self {
        self.params = params.iter().map(|s| s.as_ref().to_string()).collect();
        self
    }

    pub fn push_line<S: AsRef<str>>(&mut self, tokens: &[S]) -> &mut Self {
        self.children.push(Entry::Line(Line::from_tokens(tokens)));
        self
    }

    pub fn push_raw(&mut self, raw: impl Into<String>) -> &mut Self {
        self.children.push(Entry::Line(Line { raw: raw.into() }));
        self
    }

    pub fn push_chunk(&mut self, node: Node) -> &mut Self {
        self.children.push(Entry::Chunk(node));
        self
    }

    pub fn lines(&self) -> impl Iterator<Item = &Line> {
        self.children.iter().filter_map(|e| match e {
            Entry::Line(l) => Some(l),
            Entry::Chunk(_) => None,
        })
    }

    pub fn chunks<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter_map(move |e| match e {
            Entry::Chunk(n) if n.name == name => Some(n),
            _ => None,
        })
    }

    pub fn chunk(&self, name: &str) -> Option<&Node> {
        self.children.iter().find_map(|e| match e {
            Entry::Chunk(n) if n.name == name => Some(n),
            _ => None,
        })
    }

    /// Tokens (excluding the key) of the first line whose key is `key`.
    pub fn values(&self, key: &str) -> Option<Vec<String>> {
        self.lines().find(|l| l.key() == key).map(|l| {
            let mut t = l.tokens();
            t.remove(0);
            t
        })
    }

    pub fn value<T: std::str::FromStr>(&self, key: &str, index: usize) -> Option<T> {
        self.values(key)?.get(index)?.parse().ok()
    }

    pub fn string(&self, key: &str) -> Option<String> {
        self.values(key)?.into_iter().next()
    }
}

pub fn parse(text: &str) -> Result<Node, ParseError> {
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;

    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(header) = trimmed.strip_prefix('<') {
            let mut tokens = tokenize(header);
            let name = if tokens.is_empty() { String::new() } else { tokens.remove(0) };
            stack.push(Node { name, params: tokens, children: Vec::new() });
        } else if trimmed == ">" {
            let node = stack.pop().ok_or(ParseError::UnbalancedClose(i + 1))?;
            match stack.last_mut() {
                Some(parent) => parent.children.push(Entry::Chunk(node)),
                None => {
                    root.get_or_insert(node);
                }
            }
        } else if let Some(parent) = stack.last_mut() {
            parent.children.push(Entry::Line(Line { raw: trimmed.to_string() }));
        }
    }

    if !stack.is_empty() {
        return Err(ParseError::UnclosedChunk(stack.len()));
    }
    root.ok_or(ParseError::Empty)
}

pub fn write(root: &Node) -> String {
    let mut out = String::new();
    write_node(&mut out, root, 0);
    out
}

fn write_node(out: &mut String, node: &Node, depth: usize) {
    let indent = "  ".repeat(depth);
    let _ = write!(out, "{indent}<{}", node.name);
    if !node.params.is_empty() {
        let _ = write!(out, " {}", join_tokens(&node.params));
    }
    out.push('\n');
    for child in &node.children {
        match child {
            Entry::Line(l) => {
                let _ = writeln!(out, "{indent}  {}", l.raw);
            }
            Entry::Chunk(n) => write_node(out, n, depth + 1),
        }
    }
    let _ = writeln!(out, "{indent}>");
}

/// Split a line into tokens. Tokens are whitespace separated; a token starting with
/// `"`, `'` or `` ` `` extends to the next occurrence of the same quote character.
pub fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut tok = String::new();
        if matches!(c, '"' | '\'' | '`') {
            chars.next();
            for ch in chars.by_ref() {
                if ch == c {
                    break;
                }
                tok.push(ch);
            }
        } else {
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() {
                    break;
                }
                tok.push(ch);
                chars.next();
            }
        }
        tokens.push(tok);
    }
    tokens
}

fn quote(token: &str) -> String {
    let needs_quotes = token.is_empty()
        || token.chars().any(char::is_whitespace)
        || token.starts_with(['"', '\'', '`']);
    if !needs_quotes {
        return token.to_string();
    }
    match ['"', '\'', '`'].into_iter().find(|q| !token.contains(*q)) {
        Some(q) => format!("{q}{token}{q}"),
        // Same fallback Reaper uses: there is no escaping, so replace backticks.
        None => format!("`{}`", token.replace('`', "'")),
    }
}

fn join_tokens<S: AsRef<str>>(tokens: &[S]) -> String {
    tokens.iter().map(|t| quote(t.as_ref())).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_quotes() {
        assert_eq!(
            tokenize(r#"VST "VST3: ReaEQ (Cockos)" reaeq.vst3 0 "" 123{AB} ''"#),
            vec!["VST", "VST3: ReaEQ (Cockos)", "reaeq.vst3", "0", "", "123{AB}", ""]
        );
        assert_eq!(tokenize("NAME `say \"hi\"`"), vec!["NAME", "say \"hi\""]);
    }

    #[test]
    fn quoting_round_trips() {
        for s in ["plain", "", "with space", "it's \"x\"", "\"leading"] {
            assert_eq!(tokenize(&quote(s)), vec![s.to_string()]);
        }
    }

    #[test]
    fn parse_write_round_trip() {
        let src = "<REAPER_PROJECT 0.1 7.0/test 0\n  TEMPO 120 4 4\n  <TRACK\n    NAME \"My Track\"\n    <NOTES\n      |free text  with  spaces\n    >\n  >\n>\n";
        let tree = parse(src).unwrap();
        assert_eq!(tree.name, "REAPER_PROJECT");
        assert_eq!(tree.value::<f64>("TEMPO", 0), Some(120.0));
        assert_eq!(tree.chunk("TRACK").unwrap().string("NAME").as_deref(), Some("My Track"));
        assert_eq!(write(&tree), src);
    }
}
