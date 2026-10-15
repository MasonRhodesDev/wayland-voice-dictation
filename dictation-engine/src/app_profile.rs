//! Per-app behavioral settings derived from window class name

use crate::post_processing::{SanitizationRules, StylePreset, StyleRules};
use crate::window_detect::AppCategory;
use serde::Deserialize;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct AppProfile {
    pub category: AppCategory,
    pub word_delay_ms: u64,
    pub sanitization: SanitizationRules,
    pub style: StyleRules,
}

/// Terminal classes, matched exactly (case-sensitive) as they always were.
const TERMINAL_CLASSES: &[&str] = &[
    "kitty",
    "Alacritty",
    "foot",
    "org.wezfurlong.wezterm",
    "com.mitchellh.ghostty",
    "ghostty",
    "tmux",
];

/// Other built-in classes, lowercase, matched case-insensitively because
/// app ids differ in case between Wayland, XWayland and Flatpak builds.
const CHAT_CLASSES: &[&str] = &[
    "discord",
    "vesktop",
    "webcord",
    "legcord",
    "slack",
    "signal",
    "org.signal.signal",
    "org.telegram.desktop",
    "telegramdesktop",
    "telegram-desktop",
    "element",
    "im.riot.riot",
];
const EDITOR_CLASSES: &[&str] = &[
    "code",
    "code-oss",
    "code-url-handler",
    "codium",
    "vscodium",
    "cursor",
    "dev.zed.zed",
    "dev.zed.zed-preview",
    "zed",
    "neovide",
    "nvim-qt",
    "gvim",
    "emacs",
    "sublime_text",
];
const BROWSER_CLASSES: &[&str] = &[
    "firefox",
    "firefox-esr",
    "librewolf",
    "zen",
    "chromium",
    "google-chrome",
    "brave-browser",
    "vivaldi-stable",
    "microsoft-edge",
    "org.qutebrowser.qutebrowser",
    "org.gnome.epiphany",
];
const EMAIL_CLASSES: &[&str] = &[
    "thunderbird",
    "org.mozilla.thunderbird",
    "betterbird",
    "geary",
    "org.gnome.geary",
    "evolution",
    "org.gnome.evolution",
    "org.kde.kmail2",
];

/// Built-in category for a window class. Unlisted classes are General.
pub fn builtin_category(class: &str) -> AppCategory {
    if TERMINAL_CLASSES.contains(&class) {
        return AppCategory::Terminal;
    }
    let lower = class.to_lowercase();
    let tables = [
        (CHAT_CLASSES, AppCategory::Chat),
        (EDITOR_CLASSES, AppCategory::Editor),
        (BROWSER_CLASSES, AppCategory::Browser),
        (EMAIL_CLASSES, AppCategory::Email),
    ];
    tables
        .iter()
        .find(|(classes, _)| classes.contains(&lower.as_str()))
        .map_or(AppCategory::General, |(_, category)| *category)
}

/// Default writing style per category. General keeps the full pipeline, so
/// unknown classes behave as they did before categories drove style.
pub fn default_preset(category: AppCategory) -> StylePreset {
    match category {
        AppCategory::Terminal | AppCategory::Editor => StylePreset::Literal,
        AppCategory::Chat => StylePreset::Casual,
        AppCategory::Browser | AppCategory::Email | AppCategory::General => StylePreset::Formal,
    }
}

impl AppProfile {
    pub fn for_category(category: AppCategory) -> Self {
        Self::with_style(category, default_preset(category).rules())
    }

    fn with_style(category: AppCategory, style: StyleRules) -> Self {
        let word_delay_ms = match category {
            AppCategory::Terminal => 50,
            _ => 0,
        };
        Self {
            category,
            word_delay_ms,
            sanitization: SanitizationRules::for_category(category),
            style,
        }
    }

    #[allow(dead_code)] // planned per-app profiles
    pub fn detect() -> Self {
        // Synchronous fallback using window_detect
        Self::for_category(AppCategory::General)
    }
}

/// `[app_styles]` config section. Strings are parsed leniently: a bad entry
/// is logged and ignored, never fatal, so a typo cannot fail the config load.
#[derive(Debug, Default, Deserialize)]
pub struct AppStylesConfig {
    /// `class=category` pairs, comma-separated, e.g. `"obsidian=editor, Element=chat"`.
    #[serde(default)]
    pub overrides: Option<String>,
    #[serde(default)]
    pub terminal: Option<String>,
    #[serde(default)]
    pub editor: Option<String>,
    #[serde(default)]
    pub chat: Option<String>,
    #[serde(default)]
    pub browser: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub general: Option<String>,
}

impl AppStylesConfig {
    fn preset_for(&self, category: AppCategory) -> Option<&str> {
        match category {
            AppCategory::Terminal => self.terminal.as_deref(),
            AppCategory::Editor => self.editor.as_deref(),
            AppCategory::Chat => self.chat.as_deref(),
            AppCategory::Browser => self.browser.as_deref(),
            AppCategory::Email => self.email.as_deref(),
            AppCategory::General => self.general.as_deref(),
        }
    }
}

/// Resolved `[app_styles]`: class overrides and the preset per category.
#[derive(Debug, Clone)]
pub struct AppStyles {
    /// Lowercased class to category. Takes precedence over the built-in tables.
    overrides: Vec<(String, AppCategory)>,
    presets: [(AppCategory, StylePreset); 6],
}

impl Default for AppStyles {
    fn default() -> Self {
        Self::from_config(&AppStylesConfig::default())
    }
}

impl AppStyles {
    pub fn from_config(config: &AppStylesConfig) -> Self {
        let presets = [
            AppCategory::Terminal,
            AppCategory::Editor,
            AppCategory::Chat,
            AppCategory::Browser,
            AppCategory::Email,
            AppCategory::General,
        ]
        .map(|category| {
            let preset = match config.preset_for(category).map(str::trim) {
                None | Some("") | Some("default") => default_preset(category),
                Some(name) => StylePreset::parse(name).unwrap_or_else(|| {
                    warn!(
                        "app_styles: unknown style '{}' for {:?} ignored (known: {})",
                        name,
                        category,
                        StylePreset::NAMES.join(", ")
                    );
                    default_preset(category)
                }),
            };
            (category, preset)
        });
        let overrides = parse_overrides(config.overrides.as_deref().unwrap_or(""));
        Self { overrides, presets }
    }

    pub fn category_for(&self, class: &str) -> AppCategory {
        let lower = class.to_lowercase();
        self.overrides
            .iter()
            .find(|(c, _)| *c == lower)
            .map_or_else(|| builtin_category(class), |(_, category)| *category)
    }

    pub fn preset(&self, category: AppCategory) -> StylePreset {
        self.presets.iter().find(|(c, _)| *c == category).map_or(StylePreset::Formal, |(_, p)| *p)
    }

    /// Profile for the captured window class, or General when none was captured.
    pub fn profile(&self, class: Option<&str>) -> AppProfile {
        let category = class.map_or(AppCategory::General, |c| self.category_for(c));
        AppProfile::with_style(category, self.preset(category).rules())
    }
}

/// Parse `class=category` pairs. The first entry for a class wins.
fn parse_overrides(raw: &str) -> Vec<(String, AppCategory)> {
    let mut out: Vec<(String, AppCategory)> = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let Some((class, category)) = entry.split_once('=') else {
            warn!("app_styles: override '{}' ignored, expected class=category", entry);
            continue;
        };
        let class = class.trim().to_lowercase();
        let Some(category) = AppCategory::parse(category) else {
            warn!(
                "app_styles: override '{}' ignored, unknown category '{}' \
                 (known: terminal, editor, chat, browser, email, general)",
                entry,
                category.trim()
            );
            continue;
        };
        if class.is_empty() {
            warn!("app_styles: override '{}' ignored, empty class", entry);
        } else if out.iter().any(|(c, _)| *c == class) {
            warn!("app_styles: duplicate override for '{}' ignored", class);
        } else {
            out.push((class, category));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post_processing::StageSwitches;

    fn profile_for(class: &str) -> AppProfile {
        AppStyles::default().profile(Some(class))
    }

    #[test]
    fn builtin_mappings() {
        for (class, want) in [
            ("kitty", AppCategory::Terminal),
            ("org.wezfurlong.wezterm", AppCategory::Terminal),
            ("discord", AppCategory::Chat),
            ("Slack", AppCategory::Chat),
            ("signal", AppCategory::Chat),
            ("org.telegram.desktop", AppCategory::Chat),
            ("code", AppCategory::Editor),
            ("Code", AppCategory::Editor),
            ("dev.zed.Zed", AppCategory::Editor),
            ("neovide", AppCategory::Editor),
            ("firefox", AppCategory::Browser),
            ("chromium", AppCategory::Browser),
            ("thunderbird", AppCategory::Email),
            ("org.gnome.Geary", AppCategory::Email),
        ] {
            assert_eq!(builtin_category(class), want, "class {class}");
        }
    }

    #[test]
    fn terminal_matching_stays_exact() {
        // Formerly-unknown casings must not become terminals.
        assert_eq!(builtin_category("KITTY"), AppCategory::General);
        assert_eq!(builtin_category("alacritty"), AppCategory::General);
    }

    #[test]
    fn unknown_class_behaves_as_before() {
        for profile in [
            profile_for("some-unknown-app"),
            AppStyles::default().profile(Some("some-unknown-app")),
            AppStyles::default().profile(None),
        ] {
            assert_eq!(profile.category, AppCategory::General);
            assert_eq!(profile.word_delay_ms, 0);
            let s = &profile.sanitization;
            assert!(!s.escape_shell_chars && s.strip_control_chars && s.strip_ansi_escapes);
            assert_eq!(profile.style, StyleRules::default());
            assert!(profile.style.processor().is_none());
            let sw = profile.style.restrict(StageSwitches::default());
            assert!(sw.punctuation && sw.grammar);
        }
    }

    #[test]
    fn category_styles() {
        let terminal = profile_for("kitty");
        assert_eq!(terminal.word_delay_ms, 50);
        assert!(terminal.sanitization.escape_shell_chars);
        assert!(!terminal.style.capitalize && !terminal.style.grammar);

        let editor = profile_for("code");
        assert_eq!(editor.word_delay_ms, 0);
        assert!(!editor.style.capitalize && !editor.style.grammar);

        let chat = profile_for("discord");
        assert!(chat.style.capitalize && chat.style.grammar);
        assert!(chat.style.strip_single_sentence_period);

        for class in ["thunderbird", "firefox"] {
            assert_eq!(profile_for(class).style, StyleRules::default());
        }
    }

    #[test]
    fn same_phrase_differs_by_category() {
        let text = "Sounds good.";
        let chat = profile_for("discord").style.processor().unwrap();
        assert_eq!(chat.process(text).unwrap(), "Sounds good");
        assert!(profile_for("thunderbird").style.processor().is_none());
    }

    #[test]
    fn overrides_win_over_builtins() {
        let styles = AppStyles::from_config(&AppStylesConfig {
            overrides: Some("Obsidian=editor, discord = general, firefox=chat".into()),
            ..Default::default()
        });
        assert_eq!(styles.category_for("obsidian"), AppCategory::Editor);
        assert_eq!(styles.category_for("discord"), AppCategory::General);
        assert_eq!(styles.category_for("Firefox"), AppCategory::Chat);
        assert_eq!(styles.category_for("kitty"), AppCategory::Terminal);
        assert_eq!(styles.category_for("unknown"), AppCategory::General);
    }

    #[test]
    fn bad_overrides_are_skipped() {
        let got = parse_overrides("foo, bar=chta, =chat, baz=chat, BAZ=editor,,");
        assert_eq!(got, vec![("baz".to_string(), AppCategory::Chat)]);
    }

    #[test]
    fn preset_config() {
        let styles = AppStyles::from_config(&AppStylesConfig {
            chat: Some("formal".into()),
            terminal: Some("bogus".into()),
            editor: Some("default".into()),
            general: Some("casual".into()),
            ..Default::default()
        });
        assert_eq!(styles.preset(AppCategory::Chat), StylePreset::Formal);
        assert_eq!(styles.preset(AppCategory::Terminal), StylePreset::Literal);
        assert_eq!(styles.preset(AppCategory::Editor), StylePreset::Literal);
        assert_eq!(styles.preset(AppCategory::General), StylePreset::Casual);
        let p = styles.profile(Some("discord"));
        assert!(!p.style.strip_single_sentence_period);
    }

    #[test]
    fn config_section_deserializes() {
        let cfg: AppStylesConfig =
            toml::from_str("overrides = \"obsidian=editor\"\nchat = \"literal\"\n").unwrap();
        let styles = AppStyles::from_config(&cfg);
        assert_eq!(styles.category_for("obsidian"), AppCategory::Editor);
        assert_eq!(styles.preset(AppCategory::Chat), StylePreset::Literal);
    }
}
