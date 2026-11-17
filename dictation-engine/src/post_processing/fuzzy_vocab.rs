use super::TextProcessor;
use crate::user_dictionary::UserDictionary;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

pub struct FuzzyVocabularyProcessor {
    dictionary: Arc<UserDictionary>,
}

const MIN_FUZZY_LEN: usize = 5;
const HIGH_SIMILARITY: f64 = 0.92;
const PHONETIC_SIMILARITY: f64 = 0.82;
const MAX_JOIN: usize = 3;

struct Glossary {
    exact: HashMap<String, String>,
    fuzzy: Vec<FuzzyEntry>,
}

struct FuzzyEntry {
    canonical: String,
    normalized: String,
    phonetic: String,
}

impl FuzzyVocabularyProcessor {
    pub fn new(dictionary: Arc<UserDictionary>) -> Self {
        Self { dictionary }
    }

    fn build_glossary(&self) -> Glossary {
        let mut exact = HashMap::new();
        let mut fuzzy = Vec::new();
        for word in self.dictionary.app_words() {
            let normalized = normalize(&word);
            if normalized.is_empty() {
                continue;
            }
            exact.entry(normalized.clone()).or_insert_with(|| word.clone());
            if normalized.chars().count() >= MIN_FUZZY_LEN {
                fuzzy.push(FuzzyEntry {
                    canonical: word.clone(),
                    phonetic: phonetic_key(&normalized),
                    normalized,
                });
            }
        }
        Glossary { exact, fuzzy }
    }
}

impl TextProcessor for FuzzyVocabularyProcessor {
    fn process(&self, text: &str) -> Result<String> {
        if text.is_empty() {
            return Ok(String::new());
        }

        let glossary = self.build_glossary();
        if glossary.exact.is_empty() {
            return Ok(text.to_string());
        }

        let tokens: Vec<Token> = text.split_whitespace().map(Token::split).collect();
        let mut out: Vec<String> = Vec::with_capacity(tokens.len());
        let mut i = 0;

        while i < tokens.len() {
            let mut matched = false;
            let max_n = MAX_JOIN.min(tokens.len() - i);
            for n in (1..=max_n).rev() {
                if n > 1 && !joinable(&tokens[i..i + n]) {
                    continue;
                }
                let joined: String = tokens[i..i + n].iter().map(|t| normalize(&t.core)).collect();
                if joined.is_empty() {
                    continue;
                }
                if let Some(canonical) = glossary.exact.get(&joined) {
                    out.push(format!(
                        "{}{}{}",
                        tokens[i].prefix,
                        canonical,
                        tokens[i + n - 1].suffix
                    ));
                    i += n;
                    matched = true;
                    break;
                }
            }
            if matched {
                continue;
            }

            let token = &tokens[i];
            let norm = normalize(&token.core);
            if let Some(canonical) = best_fuzzy_match(&norm, &glossary.fuzzy) {
                out.push(format!("{}{}{}", token.prefix, canonical, token.suffix));
            } else {
                out.push(token.original.clone());
            }
            i += 1;
        }

        Ok(out.join(" "))
    }
}

struct Token {
    original: String,
    prefix: String,
    core: String,
    suffix: String,
}

impl Token {
    fn split(raw: &str) -> Self {
        let start = raw.find(|c: char| c.is_alphanumeric());
        let (prefix, rest) = match start {
            Some(s) => raw.split_at(s),
            None => (raw, ""),
        };
        let end = rest.rfind(|c: char| c.is_alphanumeric());
        let (core, suffix) = match end {
            Some(e) => rest.split_at(e + rest[e..].chars().next().map(char::len_utf8).unwrap_or(1)),
            None => ("", rest),
        };
        Token {
            original: raw.to_string(),
            prefix: prefix.to_string(),
            core: core.to_string(),
            suffix: suffix.to_string(),
        }
    }
}

fn joinable(run: &[Token]) -> bool {
    let n = run.len();
    if run[..n - 1].iter().any(|t| !t.suffix.is_empty()) {
        return false;
    }
    if run[1..].iter().any(|t| !t.prefix.is_empty()) {
        return false;
    }
    run.iter().all(|t| !t.core.is_empty())
}

fn normalize(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

fn phonetic_key(normalized: &str) -> String {
    let mut out = String::new();
    let mut last_code: Option<char> = None;
    for (idx, ch) in normalized.chars().enumerate() {
        let code = soundex_code(ch);
        if idx == 0 {
            out.push(ch);
            last_code = code;
            continue;
        }
        match code {
            Some(c) => {
                if Some(c) != last_code {
                    out.push(c);
                }
                last_code = Some(c);
            }
            None => last_code = None,
        }
    }
    out
}

fn soundex_code(c: char) -> Option<char> {
    match c.to_ascii_lowercase() {
        'b' | 'f' | 'p' | 'v' => Some('1'),
        'c' | 'g' | 'j' | 'k' | 'q' | 's' | 'x' | 'z' => Some('2'),
        'd' | 't' => Some('3'),
        'l' => Some('4'),
        'm' | 'n' => Some('5'),
        'r' => Some('6'),
        _ => None,
    }
}

fn similar_length(a: &str, b: &str) -> bool {
    let (la, lb) = (a.chars().count(), b.chars().count());
    let shorter = la.min(lb);
    la.abs_diff(lb) <= (shorter / 4).max(1)
}

fn best_fuzzy_match(norm: &str, entries: &[FuzzyEntry]) -> Option<String> {
    if norm.chars().count() < MIN_FUZZY_LEN {
        return None;
    }
    let token_phon = phonetic_key(norm);
    let mut best: Option<(f64, &str)> = None;
    for entry in entries {
        if entry.normalized == norm {
            return None;
        }
        if !similar_length(norm, &entry.normalized) {
            continue;
        }
        let sim = strsim::jaro_winkler(norm, &entry.normalized);
        let accept =
            sim >= HIGH_SIMILARITY || (token_phon == entry.phonetic && sim >= PHONETIC_SIMILARITY);
        if accept && best.map(|(b, _)| sim > b).unwrap_or(true) {
            best = Some((sim, &entry.canonical));
        }
    }
    best.map(|(_, canonical)| canonical.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_dictionary::UserDictionary;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn processor_with(words: &[&str]) -> FuzzyVocabularyProcessor {
        let mut file = NamedTempFile::new().unwrap();
        for w in words {
            writeln!(file, "{w}").unwrap();
        }
        file.flush().unwrap();
        let dict = UserDictionary::with_paths(file.path().to_path_buf(), None).unwrap();
        drop(file);
        FuzzyVocabularyProcessor::new(Arc::new(dict))
    }

    #[test]
    fn common_word_is_not_snapped_to_longer_prefix_term() {
        let p = processor_with(&["stardust", "hyprland"]);
        assert_eq!(p.process("let's start a new scheme").unwrap(), "let's start a new scheme");
        assert_eq!(p.process("open hyperland config").unwrap(), "open hyprland config");
    }

    #[test]
    fn empty_input() {
        let p = processor_with(&["lifemd"]);
        assert_eq!(p.process("").unwrap(), "");
    }

    #[test]
    fn passthrough_without_glossary() {
        let p = processor_with(&[]);
        assert_eq!(p.process("hello world").unwrap(), "hello world");
    }

    #[test]
    fn normalizes_to_dictionary_form() {
        let p = processor_with(&["lifemd"]);
        assert_eq!(p.process("i work at LifeMD today").unwrap(), "i work at lifemd today");
    }

    #[test]
    fn split_word_join() {
        let p = processor_with(&["lifemd"]);
        assert_eq!(p.process("open life md now").unwrap(), "open lifemd now");
    }

    #[test]
    fn hyphenated_multipart_join() {
        let p = processor_with(&["aws-agent-tools"]);
        assert_eq!(p.process("run aws agent tools please").unwrap(), "run aws-agent-tools please");
    }

    #[test]
    fn fuzzy_spelling_variant() {
        let p = processor_with(&["hyprland"]);
        assert_eq!(p.process("using hyperland here").unwrap(), "using hyprland here");
    }

    #[test]
    fn short_words_not_fuzzy_matched() {
        let p = processor_with(&["ad"]);
        assert_eq!(p.process("please add this").unwrap(), "please add this");
    }

    #[test]
    fn preserves_trailing_punctuation() {
        let p = processor_with(&["chezmoi"]);
        assert_eq!(p.process("edit chezmoy, now").unwrap(), "edit chezmoi, now");
    }

    #[test]
    fn leaves_unrelated_words_alone() {
        let p = processor_with(&["kubernetes"]);
        assert_eq!(p.process("the weather is nice").unwrap(), "the weather is nice");
    }
}
