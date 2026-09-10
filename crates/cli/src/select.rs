//! Small, terminal-safe selection prompts.
//!
//! The prompt implementation belongs to `inquire`. Keeping this adapter small
//! gives the rest of the CLI a stable `io::Result` API and, importantly, keeps
//! non-interactive invocations from guessing a choice.

use inquire::{error::InquireError, Select};
use std::io::{self, IsTerminal};

/// Ask the user to pick one of `options`, returning its index.
///
/// Returns `None` when the user cancels with Esc or Ctrl-C. Interactive
/// selection is intentionally unavailable when stdin or stderr is not a TTY;
/// callers should use explicit command-line flags in that case.
pub fn select(title: &str, options: &[&str], hint: &str) -> io::Result<Option<usize>> {
    if options.is_empty() {
        return Ok(None);
    }
    require_interactive_terminal()?;

    let option_values: Vec<String> = options.iter().map(|option| (*option).to_owned()).collect();
    let prompt = Select::new(title, option_values.clone())
        .with_help_message(hint)
        .prompt();
    match prompt {
        Ok(choice) => Ok(Some(choice_index(&choice, &option_values)?)),
        Err(error) => map_inquire_error(error),
    }
}

fn choice_index(choice: &str, options: &[String]) -> io::Result<usize> {
    options
        .iter()
        .position(|option| option == choice)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "prompt returned an unknown choice",
            )
        })
}

pub(crate) fn require_interactive_terminal() -> io::Result<()> {
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::NotConnected,
        "interactive selection requires a terminal; pass explicit CLI options when stdin or stderr is redirected",
    ))
}

pub(crate) fn map_inquire_error<T>(error: InquireError) -> io::Result<Option<T>> {
    match error {
        InquireError::OperationCanceled | InquireError::OperationInterrupted => Ok(None),
        InquireError::IO(error) => Err(error),
        InquireError::NotTTY => Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "interactive selection requires a terminal; pass explicit CLI options instead",
        )),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            other.to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_options_are_a_cancelled_selection() {
        assert_eq!(select("unused", &[], "unused").unwrap(), None);
    }

    #[test]
    fn unknown_prompt_choice_is_an_error() {
        let error = choice_index("missing", &["one".to_owned()]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn cancellation_is_not_an_io_failure() {
        let result: io::Result<Option<usize>> = map_inquire_error(InquireError::OperationCanceled);
        assert_eq!(result.unwrap(), None);
    }
}
