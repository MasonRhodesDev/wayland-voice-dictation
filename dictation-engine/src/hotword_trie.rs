use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tracing::{debug, info, warn};

const DEFAULT_BOOST_SCORE: f32 = 2.0;

#[derive(Debug, Clone)]
struct TrieNode {
    children: HashMap<u32, TrieNode>,
    is_terminal: Option<f32>,
    partial_boost: f32,
}

impl TrieNode {
    fn new() -> Self {
        Self { children: HashMap::new(), is_terminal: None, partial_boost: 0.0 }
    }
}

#[derive(Debug, Clone)]
pub struct HotwordEntry {
    pub text: String,
    pub boost_score: f32,
}

#[derive(Debug, Clone)]
pub struct HotwordTrie {
    root: TrieNode,
    num_hotwords: usize,
}

impl HotwordTrie {
    pub fn empty() -> Self {
        Self { root: TrieNode::new(), num_hotwords: 0 }
    }

    pub fn from_token_sequences(entries: &[(HotwordEntry, Vec<u32>)]) -> Self {
        let mut trie = Self::empty();

        for (entry, token_ids) in entries {
            if token_ids.is_empty() {
                warn!("Hotword '{}' produced no tokens, skipping", entry.text);
                continue;
            }

            let per_token_boost = entry.boost_score / token_ids.len() as f32;
            let mut node = &mut trie.root;

            for &token_id in token_ids {
                node = node.children.entry(token_id).or_insert_with(TrieNode::new);
                if per_token_boost > node.partial_boost {
                    node.partial_boost = per_token_boost;
                }
            }

            node.is_terminal = Some(entry.boost_score);
            trie.num_hotwords += 1;
        }

        info!("Built hotword trie with {} hotwords", trie.num_hotwords);
        trie
    }

    pub fn is_empty(&self) -> bool {
        self.num_hotwords == 0
    }

    pub fn len(&self) -> usize {
        self.num_hotwords
    }

    pub fn query(&self, token_ids: &[u32]) -> TrieMatch {
        let mut node = &self.root;
        let mut cumulative_boost = 0.0;

        for &token_id in token_ids {
            match node.children.get(&token_id) {
                Some(child) => {
                    cumulative_boost += child.partial_boost;
                    node = child;
                }
                None => {
                    return TrieMatch { boost: 0.0, is_prefix: false, is_complete: false };
                }
            }
        }

        TrieMatch {
            boost: cumulative_boost,
            is_prefix: !node.children.is_empty(),
            is_complete: node.is_terminal.is_some(),
        }
    }

    pub fn boost_for_token(&self, prefix_token_ids: &[u32], next_token: u32) -> f32 {
        let mut node = &self.root;

        for &token_id in prefix_token_ids {
            match node.children.get(&token_id) {
                Some(child) => node = child,
                None => return 0.0,
            }
        }

        match node.children.get(&next_token) {
            Some(child) => child.partial_boost,
            None => 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrieMatch {
    pub boost: f32,
    pub is_prefix: bool,
    pub is_complete: bool,
}

pub fn parse_hotwords_file(path: &Path) -> Result<Vec<HotwordEntry>> {
    let content = fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read hotwords file {}: {}", path.display(), e))?;

    let mut entries = Vec::new();

    for (line_num, line) in content.lines().enumerate() {
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let entry = if let Some((text, score_str)) = line.rsplit_once(' ') {
            if let Ok(score) = score_str.parse::<f32>() {
                HotwordEntry { text: text.to_string(), boost_score: score }
            } else {
                HotwordEntry { text: line.to_string(), boost_score: DEFAULT_BOOST_SCORE }
            }
        } else {
            HotwordEntry { text: line.to_string(), boost_score: DEFAULT_BOOST_SCORE }
        };

        debug!(
            "Parsed hotword (line {}): '{}' boost={}",
            line_num + 1,
            entry.text,
            entry.boost_score
        );
        entries.push(entry);
    }

    info!("Loaded {} hotwords from {}", entries.len(), path.display());
    Ok(entries)
}

pub fn default_hotwords_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("voice-dictation")
        .join("hotwords.txt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_parse_hotwords_with_scores() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hotwords.txt");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "chezmoi 5.0").unwrap();
        writeln!(f, "kubectl 3.0").unwrap();
        writeln!(f, "kubernetes").unwrap();
        writeln!(f, "# comment").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "nixos 4.5").unwrap();

        let entries = parse_hotwords_file(&path).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].text, "chezmoi");
        assert_eq!(entries[0].boost_score, 5.0);
        assert_eq!(entries[1].text, "kubectl");
        assert_eq!(entries[1].boost_score, 3.0);
        assert_eq!(entries[2].text, "kubernetes");
        assert_eq!(entries[2].boost_score, DEFAULT_BOOST_SCORE);
        assert_eq!(entries[3].text, "nixos");
        assert_eq!(entries[3].boost_score, 4.5);
    }

    #[test]
    fn test_trie_construction_and_query() {
        let entries = vec![
            (HotwordEntry { text: "kubectl".to_string(), boost_score: 3.0 }, vec![10, 20, 30]),
            (
                HotwordEntry { text: "kubernetes".to_string(), boost_score: 4.0 },
                vec![10, 20, 40, 50],
            ),
        ];

        let trie = HotwordTrie::from_token_sequences(&entries);
        assert_eq!(trie.len(), 2);

        let m = trie.query(&[10, 20, 30]);
        assert!(m.is_complete);

        let m = trie.query(&[10, 20]);
        assert!(m.is_prefix);
        assert!(!m.is_complete);
        assert!(m.boost > 0.0);

        let m = trie.query(&[99, 100]);
        assert!(!m.is_prefix);
        assert!(!m.is_complete);
        assert_eq!(m.boost, 0.0);
    }

    #[test]
    fn test_trie_empty() {
        let trie = HotwordTrie::empty();
        assert!(trie.is_empty());
        assert_eq!(trie.len(), 0);

        let m = trie.query(&[1, 2, 3]);
        assert_eq!(m.boost, 0.0);
    }

    #[test]
    fn test_boost_for_token() {
        let entries =
            vec![(HotwordEntry { text: "test".to_string(), boost_score: 6.0 }, vec![10, 20, 30])];
        let trie = HotwordTrie::from_token_sequences(&entries);

        let boost = trie.boost_for_token(&[], 10);
        assert!(boost > 0.0);

        let boost = trie.boost_for_token(&[10], 20);
        assert!(boost > 0.0);

        let boost = trie.boost_for_token(&[10], 99);
        assert_eq!(boost, 0.0);
    }
}
