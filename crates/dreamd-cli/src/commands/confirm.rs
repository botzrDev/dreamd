//! One y/N decision for every destructive confirm.
//!
//! `dreamd uninstall`, `dreamd service uninstall --purge`, and `dreamd reset
//! workspace` all call [`confirm_destructive`]. Each command keeps its own
//! prompt. The three refusal strings live only in [`ConfirmError`]'s
//! `Display`. This function does not print and does not read stdin: the
//! caller passes a `read` closure so a test never blocks on a real terminal.

use std::io;

/// Why a destructive confirm did not proceed.
#[derive(Debug)]
pub enum ConfirmError {
    /// No `--yes`, and stdin is not a terminal.
    NotATty,
    /// The answer was not `y` or `Y`.
    Declined,
    /// The prompt could not be written or the answer could not be read.
    Io(io::Error),
}

impl From<io::Error> for ConfirmError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl std::fmt::Display for ConfirmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotATty => write!(f, "stdin is not a tty; pass --yes to confirm"),
            Self::Declined => write!(f, "uninstall declined"),
            Self::Io(source) => fmt_io(source, f),
        }
    }
}

impl std::error::Error for ConfirmError {}

/// The `Io` sentence, shared so a caller that already holds the `io::Error`
/// can forward the same words without formatting them itself.
pub(crate) fn fmt_io(source: &io::Error, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "could not read the confirmation from stdin: {source}")
}

/// `yes` skips the prompt and does not call `read`.
///
/// Otherwise `read(prompt)` answers: `Ok(None)` is not a tty, `Ok(Some(line))`
/// proceeds only when `line.trim()` is `y` or `Y`, and every other answer
/// (including empty and `yes`) is [`ConfirmError::Declined`].
pub fn confirm_destructive(
    yes: bool,
    prompt: &str,
    read: &mut dyn FnMut(&str) -> Result<Option<String>, ConfirmError>,
) -> Result<(), ConfirmError> {
    if yes {
        return Ok(());
    }
    let Some(line) = read(prompt)? else {
        return Err(ConfirmError::NotATty);
    };
    let answer = line.trim();
    if answer == "y" || answer == "Y" {
        return Ok(());
    }
    Err(ConfirmError::Declined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_copy() {
        let io = ConfirmError::Io(io::Error::other("closed"));
        insta::assert_snapshot!(
            "confirmation_copy",
            format!(
                "{}\n{}\n{io}",
                ConfirmError::NotATty,
                ConfirmError::Declined
            )
        );
    }

    #[test]
    fn yes_does_not_read_and_only_y_proceeds() {
        let mut calls = 0;
        let mut read = |_: &str| {
            calls += 1;
            Ok(Some("no\n".into()))
        };
        assert!(confirm_destructive(true, "prompt", &mut read).is_ok());
        assert_eq!(calls, 0, "--yes must not touch the reader");

        let mut read = |_: &str| Ok(None);
        assert!(matches!(
            confirm_destructive(false, "prompt", &mut read),
            Err(ConfirmError::NotATty)
        ));
        for answer in ["n\n", "yes\n", "\n", " y \n"] {
            let mut read = |_: &str| Ok(Some(answer.into()));
            let got = confirm_destructive(false, "prompt", &mut read);
            if answer == " y \n" {
                assert!(got.is_ok(), "{answer:?}");
            } else {
                assert!(matches!(got, Err(ConfirmError::Declined)), "{answer:?}");
            }
        }
        let mut read = |_: &str| Ok(Some("Y\n".into()));
        assert!(confirm_destructive(false, "p", &mut read).is_ok());
    }
}
