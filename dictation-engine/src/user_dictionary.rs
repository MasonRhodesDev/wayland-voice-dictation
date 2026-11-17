use anyhow::Result;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub struct UserDictionary {
    app_words: Arc<RwLock<HashSet<String>>>,
    system_words: Arc<RwLock<HashSet<String>>>,
    app_words_path: PathBuf,
    system_dict_path: Option<PathBuf>,
}

impl UserDictionary {
    pub fn empty() -> Self {
        Self {
            app_words: Arc::new(RwLock::new(HashSet::new())),
            system_words: Arc::new(RwLock::new(HashSet::new())),
            app_words_path: PathBuf::new(),
            system_dict_path: None,
        }
    }

    pub fn new() -> Result<Self> {
        Self::with_paths(Self::get_app_words_path()?, Self::get_hunspell_personal_dict_path())
    }

    pub fn with_paths(app_words_path: PathBuf, system_dict_path: Option<PathBuf>) -> Result<Self> {
        let app_words = Self::load_app_words(&app_words_path)?;

        let system_words = if let Some(ref path) = system_dict_path {
            Self::load_system_words_from_path(path).unwrap_or_default()
        } else {
            HashSet::new()
        };

        Ok(Self {
            app_words: Arc::new(RwLock::new(app_words)),
            system_words: Arc::new(RwLock::new(system_words)),
            app_words_path,
            system_dict_path,
        })
    }

    pub fn watch_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![self.app_words_path.clone()];
        if let Some(ref system_path) = self.system_dict_path {
            paths.push(system_path.clone());
        }
        paths
    }

    pub fn contains(&self, word: &str) -> bool {
        let word_lower = word.to_lowercase();

        if let Ok(app_words) = self.app_words.read() {
            if app_words.contains(&word_lower) {
                return true;
            }
        }

        if let Ok(system_words) = self.system_words.read() {
            if system_words.contains(&word_lower) {
                return true;
            }
        }

        false
    }

    pub fn add(&self, word: &str) -> Result<()> {
        let word_lower = word.trim().to_lowercase();

        if word_lower.is_empty() {
            return Ok(());
        }

        {
            let mut app_words =
                self.app_words.write().map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
            app_words.insert(word_lower);
        }

        self.save()
    }

    pub fn remove(&self, word: &str) -> Result<()> {
        let word_lower = word.to_lowercase();

        {
            let mut app_words =
                self.app_words.write().map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
            app_words.remove(&word_lower);
        }

        self.save()
    }

    pub fn app_words(&self) -> Vec<String> {
        self.app_words
            .read()
            .map(|words| {
                let mut list: Vec<_> = words.iter().cloned().collect();
                list.sort();
                list
            })
            .unwrap_or_default()
    }

    pub fn reload_app_words(&self) -> Result<()> {
        let words = Self::load_app_words(&self.app_words_path)?;
        let mut app_words =
            self.app_words.write().map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
        *app_words = words;
        Ok(())
    }

    pub fn reload_system_words(&self) -> Result<()> {
        if let Some(ref path) = self.system_dict_path {
            let words = Self::load_system_words_from_path(path).unwrap_or_default();
            let mut system_words =
                self.system_words.write().map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;
            *system_words = words;
        }
        Ok(())
    }

    pub fn reload_all(&self) -> Result<()> {
        self.reload_app_words()?;
        self.reload_system_words()?;
        Ok(())
    }

    fn get_app_words_path() -> Result<PathBuf> {
        let data_dir = dirs::data_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not determine data directory"))?;

        let path = data_dir.join("voice-dictation").join("user_words.txt");

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        Ok(path)
    }

    fn load_app_words(path: &Path) -> Result<HashSet<String>> {
        if !path.exists() {
            return Ok(HashSet::new());
        }

        let content = fs::read_to_string(path)?;
        let words = content
            .lines()
            .map(|line| line.trim().to_lowercase())
            .filter(|line| !line.is_empty())
            .collect();

        Ok(words)
    }

    fn load_system_words_from_path(path: &Path) -> Result<HashSet<String>> {
        let content = fs::read_to_string(path)?;
        let words = content
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with('*'))
            .map(|line| line.split('/').next().unwrap_or(line).to_lowercase())
            .collect();

        Ok(words)
    }

    fn get_hunspell_personal_dict_path() -> Option<PathBuf> {
        use std::env;

        let home = dirs::home_dir()?;

        let locale = env::var("DICTIONARY")
            .or_else(|_| env::var("LC_ALL"))
            .or_else(|_| env::var("LC_MESSAGES"))
            .or_else(|_| env::var("LANG"))
            .ok()
            .and_then(|s| s.split('.').next().map(String::from));

        if let Some(loc) = locale {
            let path = home.join(format!(".hunspell_{}", loc));
            if path.exists() {
                return Some(path);
            }
        }

        let default_path = home.join(".hunspell_default");
        if default_path.exists() {
            Some(default_path)
        } else {
            None
        }
    }

    fn save(&self) -> Result<()> {
        let app_words =
            self.app_words.read().map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        let mut words: Vec<_> = app_words.iter().cloned().collect();
        words.sort();

        let content = words.join("\n");
        fs::write(&self.app_words_path, content)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_dict(dir: &TempDir) -> UserDictionary {
        UserDictionary::with_paths(dir.path().join("user_words.txt"), None).unwrap()
    }

    #[test]
    fn test_with_paths_nonexistent_file_starts_empty() {
        let dir = TempDir::new().unwrap();
        let dict = make_dict(&dir);
        assert!(dict.app_words().is_empty());
    }

    #[test]
    fn test_add_and_contains() {
        let dir = TempDir::new().unwrap();
        let dict = make_dict(&dir);
        assert!(!dict.contains("testword"));

        dict.add("testword").unwrap();
        assert!(dict.contains("testword"));
        assert!(dict.contains("TestWord"));
    }

    #[test]
    fn test_remove() {
        let dir = TempDir::new().unwrap();
        let dict = make_dict(&dir);
        dict.add("testword").unwrap();
        assert!(dict.contains("testword"));

        dict.remove("testword").unwrap();
        assert!(!dict.contains("testword"));
    }

    #[test]
    fn test_app_words_sorted() {
        let dir = TempDir::new().unwrap();
        let dict = make_dict(&dir);
        dict.add("zebra").unwrap();
        dict.add("apple").unwrap();
        dict.add("monkey").unwrap();

        let words = dict.app_words();
        assert_eq!(words, vec!["apple", "monkey", "zebra"]);
    }

    #[test]
    fn test_empty_word_ignored() {
        let dir = TempDir::new().unwrap();
        let dict = make_dict(&dir);
        assert!(dict.add("").is_ok());
        assert!(dict.add("   ").is_ok());
        assert!(dict.app_words().is_empty());
    }

    #[test]
    fn test_persistence_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("user_words.txt");

        {
            let dict = UserDictionary::with_paths(path.clone(), None).unwrap();
            dict.add("chezmoi").unwrap();
            dict.add("kubectl").unwrap();
        }

        let dict = UserDictionary::with_paths(path, None).unwrap();
        assert_eq!(dict.app_words(), vec!["chezmoi", "kubectl"]);
    }

    #[test]
    fn test_system_words_loaded_and_readonly() {
        let dir = TempDir::new().unwrap();
        let system_path = dir.path().join("hunspell_en_US");
        fs::write(&system_path, "Hyprland/S\nwayland\n*ignored\n").unwrap();

        let dict = UserDictionary::with_paths(dir.path().join("user_words.txt"), Some(system_path))
            .unwrap();

        assert!(dict.contains("hyprland"));
        assert!(dict.contains("wayland"));
        assert!(!dict.contains("ignored"));

        assert!(dict.app_words().is_empty());
    }

    #[test]
    fn test_reload_app_words_picks_up_external_edit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("user_words.txt");
        let dict = UserDictionary::with_paths(path.clone(), None).unwrap();
        assert!(!dict.contains("pipewire"));

        fs::write(&path, "pipewire\n").unwrap();
        dict.reload_app_words().unwrap();
        assert!(dict.contains("pipewire"));
    }
}
