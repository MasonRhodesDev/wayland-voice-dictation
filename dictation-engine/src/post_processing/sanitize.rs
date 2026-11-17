use super::TextProcessor;
use crate::window_detect::AppCategory;
use anyhow::Result;
use tracing::debug;

#[derive(Debug, Clone)]
pub struct SanitizationRules {
    pub escape_shell_chars: bool,
    pub strip_control_chars: bool,
    pub strip_ansi_escapes: bool,
}

impl SanitizationRules {
    pub fn for_category(category: AppCategory) -> Self {
        match category {
            AppCategory::Terminal => Self {
                escape_shell_chars: true,
                strip_control_chars: true,
                strip_ansi_escapes: true,
            },
            AppCategory::Editor => Self {
                escape_shell_chars: false,
                strip_control_chars: true,
                strip_ansi_escapes: true,
            },
            AppCategory::Browser | AppCategory::Chat | AppCategory::General => Self {
                escape_shell_chars: false,
                strip_control_chars: true,
                strip_ansi_escapes: true,
            },
        }
    }
}

pub struct SanitizationProcessor {
    rules: SanitizationRules,
    category: AppCategory,
}

impl SanitizationProcessor {
    pub fn new(rules: SanitizationRules, category: AppCategory) -> Self {
        Self { rules, category }
    }

    pub fn for_category(category: AppCategory) -> Self {
        Self { rules: SanitizationRules::for_category(category), category }
    }
}

impl TextProcessor for SanitizationProcessor {
    fn process(&self, text: &str) -> Result<String> {
        let mut result = text.to_string();
        let original_len = result.len();

        if self.rules.strip_ansi_escapes {
            result = strip_ansi_escapes(&result);
        }

        if self.rules.strip_control_chars {
            result = strip_control_chars(&result);
        }

        if self.rules.escape_shell_chars {
            result = escape_shell_chars(&result);
        }

        if result.len() != original_len {
            debug!(
                "Sanitized text for {:?}: {} -> {} chars",
                self.category,
                original_len,
                result.len()
            );
        }

        Ok(result)
    }
}

fn strip_ansi_escapes(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            if chars.peek() == Some(&']') {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
                continue;
            }
            continue;
        }
        result.push(ch);
    }

    result
}

fn strip_control_chars(text: &str) -> String {
    text.chars()
        .filter(|&ch| {
            if ch == '\n' || ch == '\t' || ch == '\r' || ch == ' ' {
                return true;
            }
            if ch.is_control() {
                return false;
            }
            if matches!(ch, '\u{200B}'..='\u{200D}' | '\u{FEFF}' | '\u{00AD}') {
                return false;
            }
            if matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{061C}') {
                return false;
            }
            if matches!(ch, '\u{FE00}'..='\u{FE0F}') {
                return false;
            }
            if matches!(ch, '\u{180E}' | '\u{200E}' | '\u{200F}') {
                return false;
            }
            true
        })
        .collect()
}

fn escape_shell_chars(text: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);

    for ch in text.chars() {
        match ch {
            '$' => result.push_str("\\$"),
            '`' => result.push_str("\\`"),
            '\\' => result.push_str("\\\\"),
            '!' => result.push_str("\\!"),
            _ => result.push(ch),
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_control_chars() {
        assert_eq!(strip_control_chars("hello\x00world"), "helloworld");
        assert_eq!(strip_control_chars("hello\x07bell"), "hellobell");
        assert_eq!(strip_control_chars("hello\nworld"), "hello\nworld");
        assert_eq!(strip_control_chars("hello\tworld"), "hello\tworld");

        assert_eq!(strip_control_chars("hello\u{200B}world"), "helloworld");
        assert_eq!(strip_control_chars("hello\u{FEFF}world"), "helloworld");
        assert_eq!(strip_control_chars("hello\u{00AD}world"), "helloworld");

        assert_eq!(strip_control_chars("hello\u{202A}world"), "helloworld");
        assert_eq!(strip_control_chars("hello\u{202E}world"), "helloworld");
        assert_eq!(strip_control_chars("hello\u{2066}world"), "helloworld");
    }

    #[test]
    fn test_strip_ansi_escapes() {
        assert_eq!(strip_ansi_escapes("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi_escapes("\x1b[1;32mbold green\x1b[0m"), "bold green");
        assert_eq!(strip_ansi_escapes("\x1b]0;title\x07text"), "text");
    }

    #[test]
    fn test_escape_shell_chars() {
        assert_eq!(escape_shell_chars("echo $HOME"), "echo \\$HOME");
        assert_eq!(escape_shell_chars("echo `date`"), "echo \\`date\\`");
        assert_eq!(escape_shell_chars("path\\to\\file"), "path\\\\to\\\\file");
        assert_eq!(escape_shell_chars("wow!"), "wow\\!");
    }

    #[test]
    fn test_terminal_sanitization() {
        let processor = SanitizationProcessor::for_category(AppCategory::Terminal);

        let result = processor.process("echo $HOME").unwrap();
        assert_eq!(result, "echo \\$HOME");

        let result = processor.process("hello\x00world").unwrap();
        assert_eq!(result, "helloworld");

        let result = processor.process("\x1b[31mred\x1b[0m text").unwrap();
        assert_eq!(result, "red text");
    }

    #[test]
    fn test_general_sanitization() {
        let processor = SanitizationProcessor::for_category(AppCategory::General);

        let result = processor.process("echo $HOME").unwrap();
        assert_eq!(result, "echo $HOME");

        let result = processor.process("hello\x00world").unwrap();
        assert_eq!(result, "helloworld");
    }

    #[test]
    fn test_empty_input() {
        let processor = SanitizationProcessor::for_category(AppCategory::Terminal);
        let result = processor.process("").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_whitespace_only() {
        let processor = SanitizationProcessor::for_category(AppCategory::Terminal);

        assert_eq!(processor.process("   ").unwrap(), "   ");

        assert_eq!(processor.process("\t\t").unwrap(), "\t\t");

        assert_eq!(processor.process("\n\n").unwrap(), "\n\n");
    }

    #[test]
    fn test_unicode_emoji_passthrough() {
        let processor = SanitizationProcessor::for_category(AppCategory::Terminal);

        let result = processor.process("hello 👋 world").unwrap();
        assert!(result.contains("👋"));

        let result = processor.process("test 🎉🎊🎁 done").unwrap();
        assert!(result.contains("🎉"));
        assert!(result.contains("🎊"));
    }

    #[test]
    fn test_variation_selector_stripped() {
        let processor = SanitizationProcessor::for_category(AppCategory::Terminal);

        let with_selector = "test\u{FE0F}text";
        let result = processor.process(with_selector).unwrap();
        assert_eq!(result, "testtext");
    }

    #[test]
    fn test_all_bidi_marks_stripped() {
        let processor = SanitizationProcessor::for_category(AppCategory::Terminal);

        let marks = [
            '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}',
            '\u{2068}', '\u{2069}', '\u{061C}',
        ];

        for mark in marks {
            let input = format!("hello{}world", mark);
            let result = processor.process(&input).unwrap();
            assert_eq!(result, "helloworld", "Failed to strip {:?}", mark);
        }
    }

    #[test]
    fn test_complex_ansi_sequences() {
        let processor = SanitizationProcessor::for_category(AppCategory::Terminal);

        let result = processor.process("\x1b[38;5;196mred\x1b[0m").unwrap();
        assert_eq!(result, "red");

        let result = processor.process("\x1b[38;2;255;0;0mred\x1b[0m").unwrap();
        assert_eq!(result, "red");

        let result = processor.process("\x1b[1m\x1b[31mbold red\x1b[0m\x1b[0m").unwrap();
        assert_eq!(result, "bold red");
    }

    #[test]
    fn test_osc_sequence_with_st() {
        let result = strip_ansi_escapes("\x1b]0;title\x1b\\text");
        assert_eq!(result, "text");
    }

    #[test]
    fn test_mixed_problematic_chars() {
        let processor = SanitizationProcessor::for_category(AppCategory::Terminal);

        let input = "\x1b[31m$HOME\u{200B}\x00test\u{202E}!\x1b[0m";
        let result = processor.process(input).unwrap();

        assert!(result.contains("\\$HOME"));
        assert!(result.contains("test"));
        assert!(result.contains("\\!"));
        assert!(!result.contains("\x1b"));
        assert!(!result.contains("\x00"));
    }

    #[test]
    fn test_all_shell_chars_escaped() {
        let processor = SanitizationProcessor::for_category(AppCategory::Terminal);

        let input = "echo $VAR `cmd` path\\file wow!";
        let result = processor.process(input).unwrap();

        assert_eq!(result, "echo \\$VAR \\`cmd\\` path\\\\file wow\\!");
    }

    #[test]
    fn test_long_text_sanitization() {
        let processor = SanitizationProcessor::for_category(AppCategory::Terminal);

        let mut input = String::new();
        for i in 0..1000 {
            if i % 100 == 0 {
                input.push_str("$HOME ");
            } else if i % 50 == 0 {
                input.push('\u{200B}');
            } else {
                input.push_str("word ");
            }
        }

        let result = processor.process(&input).unwrap();

        assert!(result.contains("\\$HOME"));
        assert!(!result.contains('\u{200B}'));
    }

    #[test]
    fn test_category_rules() {
        let terminal = SanitizationRules::for_category(AppCategory::Terminal);
        assert!(terminal.escape_shell_chars);
        assert!(terminal.strip_control_chars);
        assert!(terminal.strip_ansi_escapes);

        let editor = SanitizationRules::for_category(AppCategory::Editor);
        assert!(!editor.escape_shell_chars);
        assert!(editor.strip_control_chars);

        let browser = SanitizationRules::for_category(AppCategory::Browser);
        assert!(!browser.escape_shell_chars);

        let chat = SanitizationRules::for_category(AppCategory::Chat);
        assert!(!chat.escape_shell_chars);

        let general = SanitizationRules::for_category(AppCategory::General);
        assert!(!general.escape_shell_chars);
    }

    #[test]
    fn test_format_chars_stripped() {
        let processor = SanitizationProcessor::for_category(AppCategory::Terminal);

        let result = processor.process("hello\u{180E}world").unwrap();
        assert_eq!(result, "helloworld");

        let result = processor.process("hello\u{200E}world").unwrap();
        assert_eq!(result, "helloworld");

        let result = processor.process("hello\u{200F}world").unwrap();
        assert_eq!(result, "helloworld");
    }

    #[test]
    fn test_transcription_realistic_outputs() {
        let processor = SanitizationProcessor::for_category(AppCategory::Terminal);

        let inputs = [
            "Hello, my name is John.",
            "I'd like to run echo $HOME in the terminal.",
            "The command is `ls -la`.",
            "Wow! That's amazing!",
            "Let me check the file path\\config\\settings.",
        ];

        let expected = [
            "Hello, my name is John.",
            "I'd like to run echo \\$HOME in the terminal.",
            "The command is \\`ls -la\\`.",
            "Wow\\! That's amazing\\!",
            "Let me check the file path\\\\config\\\\settings.",
        ];

        for (input, expect) in inputs.iter().zip(expected.iter()) {
            let result = processor.process(input).unwrap();
            assert_eq!(&result, *expect, "Failed for input: {}", input);
        }
    }
}
