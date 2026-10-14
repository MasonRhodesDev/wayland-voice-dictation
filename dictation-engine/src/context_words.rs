//! Context biasing: distinctive words from what is on screen at recording start.
//!
//! When a recording starts, the daemon gathers a little text from the current
//! context (see [`crate::context_sources`]), keeps only the distinctive tokens
//! (identifiers and words outside the common English dictionary) and hands
//! them to the session:
//!
//! - CTC beam search (`CtcDirectEngine`) gets their spoken forms as a
//!   temporary, lower-weight hotword list.
//! - The fuzzy vocabulary stage gets them as a temporary glossary extension,
//!   and the grammar stage stops "fixing" their spelling.
//!
//! Context words live only in memory for one session. They are never written
//! to the user dictionary or any other file.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use harper_core::spell::{Dictionary, MutableDictionary};

/// Most context words kept for one session. Keeps the hotword trie small and
/// limits how many glossary entries can misfire.
pub const MAX_CONTEXT_WORDS: usize = 40;

/// Longest slice of one source that is scanned. A terminal viewport is far
/// below this; the bound only guards against a pathological source.
const MAX_SOURCE_CHARS: usize = 16_000;

/// Shortest token kept. Two-letter tokens are mostly flags and noise.
const MIN_LEN: usize = 3;

/// Longest token kept. Longer runs are hashes, base64 or paths.
const MAX_LEN: usize = 40;

/// Words that are not in the English dictionary but are everywhere in
/// terminals and window titles, so they carry no signal about the context.
const STOPWORDS: &[&str] = &[
    "bash",
    "bin",
    "cargo",
    "com",
    "config",
    "debug",
    "dev",
    "env",
    "eof",
    "err",
    "etc",
    "false",
    "fish",
    "git",
    "github",
    "http",
    "https",
    "info",
    "lib",
    "localhost",
    "mnt",
    "nil",
    "null",
    "opt",
    "org",
    "proc",
    "src",
    "stderr",
    "stdin",
    "stdout",
    "sudo",
    "sys",
    "tmp",
    "todo",
    "true",
    "txt",
    "usr",
    "var",
    "warn",
    "wezterm",
    "www",
    "zsh",
];

/// One distinctive word found in the context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextWord {
    /// The word as it appeared on screen (`WindowTarget`, `hyprctl`).
    pub text: String,
    /// How it is said: identifier parts split, lowercased (`window target`).
    pub spoken: String,
    /// Whether any part is outside the common English dictionary. Only these
    /// are worth boosting acoustically; an identifier built from common words
    /// is already recognized and only needs its written form restored.
    pub rare: bool,
}

/// The words for one session, shared read-only by the engine and pipelines.
#[derive(Debug, Clone, Default)]
pub struct ContextWords {
    words: Arc<Vec<ContextWord>>,
}

impl ContextWords {
    pub fn new(words: Vec<ContextWord>) -> Self {
        Self { words: Arc::new(words) }
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ContextWord> {
        self.words.iter()
    }

    /// Spoken forms for CTC hotword boosting (rare words only).
    pub fn hotwords(&self) -> Vec<String> {
        self.words.iter().filter(|w| w.rare).map(|w| w.spoken.clone()).collect()
    }

    /// Written forms for the fuzzy vocabulary glossary.
    pub fn glossary(&self) -> Vec<String> {
        self.words.iter().map(|w| w.text.clone()).collect()
    }

    /// Lowercased words and their spoken parts, for skipping spelling lints.
    pub fn spelling_allowlist(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for w in self.words.iter() {
            out.insert(w.text.to_lowercase());
            for part in w.spoken.split(' ') {
                out.insert(part.to_string());
            }
        }
        out
    }
}

/// Keep the distinctive tokens from `sources` (highest priority first).
///
/// Sources are taken in order, so every word from an earlier source (the
/// focused field) outranks every word from a later one (the window title). Within one source, words seen more often rank
/// first, then words seen later (the bottom of a terminal is the most recent).
/// Duplicates are dropped case- and punctuation-insensitively, keeping the
/// first spelling.
pub fn extract(sources: &[&str], cap: usize) -> Vec<ContextWord> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();

    for source in sources {
        if out.len() >= cap {
            break;
        }
        let source = bounded(source);

        // normalized -> (first spelling, count, last position)
        let mut found: HashMap<String, (ContextWord, usize, usize)> = HashMap::new();
        for (pos, token) in tokens(source).enumerate() {
            let Some(word) = classify(token) else { continue };
            let key = normalize(&word.text);
            if seen.contains(&key) {
                continue;
            }
            found
                .entry(key)
                .and_modify(|(_, count, last)| {
                    *count += 1;
                    *last = pos;
                })
                .or_insert((word, 1, pos));
        }

        let mut ranked: Vec<_> = found.into_iter().collect();
        ranked.sort_by(|(_, (_, ca, la)), (_, (_, cb, lb))| cb.cmp(ca).then(lb.cmp(la)));
        for (key, (word, _, _)) in ranked {
            if out.len() >= cap {
                break;
            }
            seen.insert(key);
            out.push(word);
        }
    }
    out
}

/// The tail of `s`, at most [`MAX_SOURCE_CHARS`] long, cut on a char boundary.
fn bounded(s: &str) -> &str {
    if s.len() <= MAX_SOURCE_CHARS {
        return s;
    }
    let mut start = s.len() - MAX_SOURCE_CHARS;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Split text into candidate tokens. `_` and `-` stay inside a token so
/// `hotword_trie` and `node-red` survive; everything else separates, so a path
/// like `src/window_target.rs` yields `src`, `window_target`, `rs`.
fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .map(|t| t.trim_matches(|c| c == '_' || c == '-'))
        .filter(|t| !t.is_empty())
}

/// Decide whether one token is distinctive enough to keep.
fn classify(token: &str) -> Option<ContextWord> {
    let len = token.chars().count();
    if !(MIN_LEN..=MAX_LEN).contains(&len) {
        return None;
    }
    let letters = token.chars().filter(|c| c.is_alphabetic()).count();
    let digits = token.chars().filter(|c| c.is_ascii_digit()).count();
    // Numbers, versions and mostly-numeric tokens (x86_64, 2024-10-14).
    if letters < 2 || digits > letters {
        return None;
    }
    if looks_like_hash(token) {
        return None;
    }
    let lower = token.to_lowercase();
    if STOPWORDS.contains(&lower.as_str()) {
        return None;
    }

    let parts = split_identifier(token);
    if parts.is_empty() {
        return None;
    }
    let rare = parts.iter().any(|p| is_rare(p));
    let identifier = is_identifier(token);
    // A plain word must be rare. An identifier made only of common words is
    // kept when it has three or more parts: "is empty" is ordinary speech,
    // "get user id" rarely is.
    let keep = rare || (identifier && parts.len() >= 3);
    if !keep {
        return None;
    }
    Some(ContextWord { text: token.to_string(), spoken: parts.join(" "), rare })
}

/// Identifier shape: snake_case, camelCase/PascalCase, or letters with digits.
fn is_identifier(token: &str) -> bool {
    if token.contains('_') {
        return true;
    }
    let chars: Vec<char> = token.chars().collect();
    let camel = chars.windows(2).any(|w| w[0].is_lowercase() && w[1].is_uppercase());
    let mixed_digits = chars.iter().any(|c| c.is_ascii_digit());
    camel || mixed_digits
}

/// Commit hashes, UUID fragments and other hex noise.
fn looks_like_hash(token: &str) -> bool {
    token.len() >= 7
        && token.chars().all(|c| c.is_ascii_hexdigit())
        && (token.len() >= 8 || token.chars().any(|c| c.is_ascii_digit()))
}

/// Split an identifier into lowercase spoken parts:
/// `WindowTarget` -> `window target`, `hotword_trie` -> `hotword trie`,
/// `HTTPServer` -> `http server`, `node-red` -> `node red`.
fn split_identifier(token: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for chunk in token.split(['_', '-']).filter(|c| !c.is_empty()) {
        let chars: Vec<char> = chunk.chars().collect();
        let mut current = String::new();
        for (i, &c) in chars.iter().enumerate() {
            let prev = i.checked_sub(1).map(|j| chars[j]);
            let next = chars.get(i + 1).copied();
            let boundary = match prev {
                None => false,
                Some(p) => {
                    (p.is_lowercase() && c.is_uppercase())
                        // End of an acronym run: the "S" in "HTTPServer".
                        || (p.is_uppercase()
                            && c.is_uppercase()
                            && next.is_some_and(|n| n.is_lowercase()))
                        || (p.is_alphabetic() != c.is_alphabetic())
                }
            };
            if boundary && !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            current.extend(c.to_lowercase());
        }
        if !current.is_empty() {
            parts.push(current);
        }
    }
    parts
}

/// A part is rare when it is a real word (3+ letters) that the curated English
/// dictionary does not know.
fn is_rare(part: &str) -> bool {
    if part.chars().count() < MIN_LEN || !part.chars().all(char::is_alphabetic) {
        return false;
    }
    if STOPWORDS.contains(&part) {
        return false;
    }
    !english().contains_word_str(part)
}

fn english() -> &'static Arc<MutableDictionary> {
    static DICT: OnceLock<Arc<MutableDictionary>> = OnceLock::new();
    DICT.get_or_init(MutableDictionary::curated)
}

/// Lowercase alphanumerics only: the dedupe key.
fn normalize(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(words: &[ContextWord]) -> Vec<&str> {
        words.iter().map(|w| w.text.as_str()).collect()
    }

    #[test]
    fn common_english_and_stopwords_are_dropped() {
        let got = extract(&["the window is open and the user said hello", "sudo git usr"], 10);
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn rare_words_and_identifiers_are_kept() {
        let got = extract(&["run kubectl apply then open hotword_trie in WindowTarget"], 10);
        let t = texts(&got);
        assert!(t.contains(&"kubectl"), "{t:?}");
        assert!(t.contains(&"hotword_trie"), "{t:?}");
        assert!(!t.contains(&"apply"), "{t:?}");
        assert!(!t.contains(&"open"), "{t:?}");
    }

    #[test]
    fn common_word_identifiers_need_three_parts() {
        // "is empty" and "window target" are ordinary speech; rewriting them
        // in prose would be a false positive.
        let got = extract(&["isEmpty WindowTarget get_user_id"], 10);
        assert_eq!(texts(&got), vec!["get_user_id"]);
        assert!(!got[0].rare);
    }

    #[test]
    fn short_numeric_and_hash_tokens_are_dropped() {
        let got = extract(&["ls -la v2 x86_64 2024-10-14 1f343c1 deadbeefcafe 42"], 10);
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn identifier_spoken_forms() {
        assert_eq!(split_identifier("WindowTarget"), vec!["window", "target"]);
        assert_eq!(split_identifier("hotword_trie"), vec!["hotword", "trie"]);
        assert_eq!(split_identifier("HTTPServer"), vec!["http", "server"]);
        assert_eq!(split_identifier("node-red"), vec!["node", "red"]);
        assert_eq!(split_identifier("hyprctl"), vec!["hyprctl"]);
        assert_eq!(split_identifier("utf8Decode"), vec!["utf", "8", "decode"]);
    }

    #[test]
    fn rare_flag_marks_acoustic_candidates() {
        let got = extract(&["hyprctl fetch_user_data"], 10);
        let hypr = got.iter().find(|w| w.text == "hyprctl").unwrap();
        assert!(hypr.rare);
        assert_eq!(hypr.spoken, "hyprctl");
        let ident = got.iter().find(|w| w.text == "fetch_user_data").unwrap();
        assert!(!ident.rare);
        assert_eq!(ident.spoken, "fetch user data");
        // Only rare words become hotwords; every kept word joins the glossary.
        let words = ContextWords::new(got);
        assert_eq!(words.hotwords(), vec!["hyprctl"]);
        assert_eq!(words.glossary().len(), 2);
    }

    #[test]
    fn duplicates_are_merged_case_insensitively() {
        let got = extract(&["Hyprland hyprland HYPRLAND", "hyprland"], 10);
        assert_eq!(texts(&got), vec!["Hyprland"]);
    }

    #[test]
    fn earlier_sources_outrank_later_ones() {
        let got = extract(&["chezmoi", "kubectl kubectl kubectl"], 10);
        assert_eq!(texts(&got), vec!["chezmoi", "kubectl"]);
    }

    #[test]
    fn frequency_then_recency_ranks_within_a_source() {
        let got = extract(&["chezmoi kubectl hyprctl kubectl"], 10);
        assert_eq!(texts(&got), vec!["kubectl", "hyprctl", "chezmoi"]);
    }

    #[test]
    fn cap_is_respected() {
        let got = extract(&["chezmoi kubectl hyprctl pipewire wezterm nixos"], 3);
        assert_eq!(got.len(), 3);
        let got = extract(&["chezmoi kubectl", "hyprctl pipewire"], 3);
        assert_eq!(texts(&got), vec!["kubectl", "chezmoi", "pipewire"]);
    }

    #[test]
    fn empty_sources() {
        assert!(extract(&[], 10).is_empty());
        assert!(extract(&["", "   \n"], 10).is_empty());
    }

    #[test]
    fn paths_split_into_components() {
        let got = extract(&["nvim ~/repos/dictation-engine/src/hotword_trie.rs"], 10);
        let t = texts(&got);
        assert!(t.contains(&"hotword_trie"), "{t:?}");
        assert!(!t.iter().any(|w| w.contains('/')), "{t:?}");
    }

    #[test]
    fn bounded_keeps_the_tail_on_a_char_boundary() {
        let s = format!("{}é{}", "a".repeat(MAX_SOURCE_CHARS), "kubectl");
        let b = bounded(&s);
        assert!(b.len() <= MAX_SOURCE_CHARS);
        assert!(b.ends_with("kubectl"));
    }

    #[test]
    fn spelling_allowlist_includes_parts() {
        let words = ContextWords::new(extract(&["hotword_trie"], 10));
        let allow = words.spelling_allowlist();
        assert!(allow.contains("hotword_trie"));
        assert!(allow.contains("hotword"));
        assert!(allow.contains("trie"));
    }
}
