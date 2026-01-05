//! WRITTEN BY CLAUDE WITH GUIDANCE
//! Nextest wrapper script that tracks test pass/fail status.
//! This binary is invoked by nextest for each test execution.
//! It runs the test, records the result, and returns the exit code.

use std::env;
use std::process::{Command, ExitCode};
use xtask::failures::{append_result, TestResult};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    // Args: [wrapper-binary, test-binary, test-args...]
    if args.len() < 2 {
        eprintln!("\x1b[1;31mnextest-failure-tracker: missing test binary argument\x1b[0m");
        return ExitCode::from(1);
    }

    let test_binary = &args[1];
    let test_args = &args[2..];

    let test_name = extract_test_name(test_args).unwrap_or_else(|| {
        eprintln!("\x1b[1;31mnextest-failure-tracker: unable to determine test name from args {:?}\x1b[0m", test_args);
        "unknown".to_string()
    });

    // Run the actual test
    let status = Command::new(test_binary).args(test_args).status();

    let (passed, exit_code) = match status {
        Ok(status) => {
            let code = status.code().unwrap_or(1);
            (code == 0, code as u8)
        }
        Err(e) => {
            eprintln!(
                "\x1b[1;31mnextest-failure-tracker: failed to run test: {}\x1b[0m\n{}",
                &test_name, e
            );
            (false, 1)
        }
    };

    let result = TestResult {
        name: test_name,
        passed,
    };

    if let Err(e) = append_result(&result) {
        eprintln!(
            "\x1b[1;31mnextest-failure-tracker: failed to record result:\x1b[0m {}",
            e
        );
    }

    ExitCode::from(exit_code)
}

/// Extract the test name from the test arguments.
///
/// nextest invokes tests like: `test-binary test_name --exact --nocapture`
/// The test name is the first argument that doesn't start with '-'.
fn extract_test_name(args: &[String]) -> Option<String> {
    for arg in args {
        if !arg.starts_with('-') {
            return Some(arg.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_test_name_basic() {
        let args = vec![
            "my_module::test_foo".to_string(),
            "--exact".to_string(),
            "--nocapture".to_string(),
        ];
        assert_eq!(
            extract_test_name(&args),
            Some("my_module::test_foo".to_string())
        );
    }

    #[test]
    fn test_extract_test_name_with_leading_flags() {
        // Unlikely, but handle it
        let args = vec!["--some-flag".to_string(), "test_name".to_string()];
        assert_eq!(extract_test_name(&args), Some("test_name".to_string()));
    }

    #[test]
    fn test_extract_test_name_empty() {
        let args: Vec<String> = vec![];
        assert_eq!(extract_test_name(&args), None);
    }

    #[test]
    fn test_extract_test_name_only_flags() {
        let args = vec!["--exact".to_string(), "--nocapture".to_string()];
        assert_eq!(extract_test_name(&args), None);
    }
}
