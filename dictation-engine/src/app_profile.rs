use crate::post_processing::SanitizationRules;
use crate::window_detect::AppCategory;

pub struct AppProfile {
    pub category: AppCategory,
    pub word_delay_ms: u64,
    pub sanitization: SanitizationRules,
}

impl AppProfile {
    pub fn for_category(category: AppCategory) -> Self {
        let (word_delay_ms, sanitization) = match category {
            AppCategory::Terminal => (50, SanitizationRules::for_category(category)),
            _ => (0, SanitizationRules::for_category(category)),
        };
        Self { category, word_delay_ms, sanitization }
    }

    pub fn from_window_class(class: &str) -> Self {
        let category = match class {
            "kitty"
            | "Alacritty"
            | "foot"
            | "org.wezfurlong.wezterm"
            | "com.mitchellh.ghostty"
            | "ghostty"
            | "tmux"
            | "com.apple.Terminal"
            | "com.googlecode.iterm2"
            | "net.kovidgoyal.kitty"
            | "org.alacritty"
            | "com.github.wez.wezterm" => AppCategory::Terminal,
            _ => AppCategory::General,
        };
        Self::for_category(category)
    }

    #[allow(dead_code)]
    pub fn detect() -> Self {
        Self::for_category(AppCategory::General)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_terminal_bundle_ids_type_slowly() {
        for bundle_id in [
            "com.apple.Terminal",
            "com.googlecode.iterm2",
            "net.kovidgoyal.kitty",
            "org.alacritty",
            "com.github.wez.wezterm",
            "com.mitchellh.ghostty",
        ] {
            let profile = AppProfile::from_window_class(bundle_id);
            assert_eq!(profile.category, AppCategory::Terminal, "{bundle_id}");
            assert!(profile.word_delay_ms > 0, "{bundle_id}");
        }
        assert_eq!(
            AppProfile::from_window_class("com.apple.Safari").category,
            AppCategory::General
        );
    }
}
