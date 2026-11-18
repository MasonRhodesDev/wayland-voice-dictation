const NOT_AUTHORIZED: &str = "(-1743)";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reply<'a> {
    Value(&'a str),
    NotAuthorized,
    Failed(&'a str),
}

pub(crate) fn parse_reply<'a>(success: bool, stdout: &'a str, stderr: &'a str) -> Reply<'a> {
    match (success, stderr.contains(NOT_AUTHORIZED)) {
        (true, _) => Reply::Value(stdout.trim()),
        (false, true) => Reply::NotAuthorized,
        (false, false) => Reply::Failed(stderr.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_runs_yield_the_trimmed_result() {
        assert_eq!(parse_reply(true, "true\n", ""), Reply::Value("true"));
        assert_eq!(parse_reply(true, "paused\n", ""), Reply::Value("paused"));
        assert_eq!(parse_reply(true, "", ""), Reply::Value(""));
    }

    #[test]
    fn missing_automation_permission_is_recognised() {
        let stderr =
            "62:80: execution error: Not authorized to send Apple events to Music. (-1743)\n";
        assert_eq!(parse_reply(false, "", stderr), Reply::NotAuthorized);
    }

    #[test]
    fn other_errors_carry_the_message() {
        let stderr = "0:41: execution error: Music got an error: AppleEvent timed out. (-1712)\n";
        assert_eq!(
            parse_reply(false, "", stderr),
            Reply::Failed(
                "0:41: execution error: Music got an error: AppleEvent timed out. (-1712)"
            )
        );
    }
}
