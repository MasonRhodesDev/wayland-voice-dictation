pub(crate) fn field_value(output: &str) -> Option<&str> {
    let value = output.trim().split_once('=')?.1.trim_matches('"');
    (!value.is_empty() && value != "[ NULL ]").then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_quoted_and_bare_values() {
        assert_eq!(
            field_value("\"CFBundleIdentifier\"=\"com.apple.Terminal\"\n"),
            Some("com.apple.Terminal")
        );
        assert_eq!(field_value("\"pid\"=4242\n"), Some("4242"));
    }

    #[test]
    fn missing_values_are_none() {
        assert_eq!(field_value("\"CFBundleIdentifier\"=[ NULL ]\n"), None);
        assert_eq!(field_value(""), None);
    }
}
