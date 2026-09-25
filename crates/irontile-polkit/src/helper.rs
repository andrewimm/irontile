//! Talking to polkit's own authentication helper.
//!
//! An agent does not check passwords. It cannot: verifying the administrator's
//! password means reading the shadow file, and an agent runs as whoever is
//! sitting at the machine. polkit ships a helper that does it instead -- these
//! days started by systemd on a socket rather than carrying a setuid bit -- and
//! the helper is also what tells the authority the answer. The cookie handed
//! over below is what ties this conversation to the request the authority is
//! waiting on; without it, being told "SUCCESS" would mean nothing to anybody.
//!
//! The protocol is lines. Write a username and a cookie, answer whatever it
//! asks, and read `SUCCESS` or `FAILURE`:
//!
//! ```text
//! -> andrew
//! -> 1-deadbeef-...
//! <- PAM_PROMPT_ECHO_OFF Password:
//! -> hunter2
//! <- SUCCESS
//! ```

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Where systemd listens on the session's behalf.
const SOCKET: &str = "/run/polkit/agent-helper.socket";

/// What the helper wanted, one line at a time.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Ask for something and send it back. `echo` is false for a password,
    /// which is the only kind of prompt anybody has seen it send.
    Ask { prompt: String, echo: bool },
    /// Something to show, which expects no answer.
    Say(String),
    /// The authority has been told, and this conversation is over.
    Done(bool),
}

/// Whether the helper is there to be asked.
///
/// Its absence is not a failure to report loudly: a machine with no polkit is
/// a machine where nothing will ever ask for an administrator, and an agent
/// that refused to start would be the only thing complaining.
pub fn available() -> bool {
    Path::new(SOCKET).exists()
}

/// One authentication, from the identity to the verdict.
pub struct Conversation {
    stream: UnixStream,
    lines: BufReader<UnixStream>,
}

impl Conversation {
    /// Opens a conversation about `user`, for the request named by `cookie`.
    pub fn open(user: &str, cookie: &str) -> std::io::Result<Conversation> {
        let stream = UnixStream::connect(SOCKET)?;
        let lines = BufReader::new(stream.try_clone()?);
        let mut conversation = Conversation { stream, lines };
        // Identity first, then the cookie: the helper reads both before it says
        // anything at all.
        writeln!(conversation.stream, "{user}")?;
        writeln!(conversation.stream, "{cookie}")?;
        conversation.stream.flush()?;
        Ok(conversation)
    }

    /// The next thing the helper has to say.
    pub fn next(&mut self) -> std::io::Result<Step> {
        let mut line = String::new();
        if self.lines.read_line(&mut line)? == 0 {
            // The helper went away without a verdict. Treating that as a
            // refusal is the only safe reading: something went wrong, and
            // nothing went wrong in a way that should let anybody through.
            return Ok(Step::Done(false));
        }
        Ok(parse(line.trim_end_matches('\n')))
    }

    /// Answers whatever it just asked.
    pub fn answer(&mut self, text: &str) -> std::io::Result<()> {
        writeln!(self.stream, "{text}")?;
        self.stream.flush()
    }
}

/// Reads one line of the helper's side of the conversation.
fn parse(line: &str) -> Step {
    match line.split_once(' ') {
        Some(("PAM_PROMPT_ECHO_OFF", prompt)) => Step::Ask {
            prompt: prompt.trim().to_string(),
            echo: false,
        },
        Some(("PAM_PROMPT_ECHO_ON", prompt)) => Step::Ask {
            prompt: prompt.trim().to_string(),
            echo: true,
        },
        Some(("PAM_ERROR_MSG", text)) | Some(("PAM_TEXT_INFO", text)) => {
            Step::Say(text.trim().to_string())
        }
        _ => match line.trim() {
            "SUCCESS" => Step::Done(true),
            // Anything else is not a yes, and only a yes is a yes.
            _ => Step::Done(false),
        },
    }
}

/// Runs one whole conversation with a single secret to offer.
///
/// Every prompt gets the same answer, which is what a dialog with one field
/// can honestly provide. A stack that asked two different questions would get
/// the same reply twice and refuse, which is the right outcome: better a
/// refusal than an agent quietly answering a question nobody was shown.
pub fn authenticate(user: &str, cookie: &str, secret: &str) -> std::io::Result<bool> {
    let mut conversation = Conversation::open(user, cookie)?;
    loop {
        match conversation.next()? {
            Step::Ask { .. } => conversation.answer(secret)?,
            Step::Say(_) => {}
            Step::Done(verdict) => return Ok(verdict),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Step, parse};

    #[test]
    fn a_password_prompt_is_recognised_and_kept_quiet() {
        assert_eq!(
            parse("PAM_PROMPT_ECHO_OFF Password: "),
            Step::Ask {
                prompt: "Password:".to_string(),
                echo: false,
            }
        );
    }

    #[test]
    fn only_success_is_success() {
        assert_eq!(parse("SUCCESS"), Step::Done(true));
        assert_eq!(parse("FAILURE"), Step::Done(false));
        // Whatever this is, it is not the helper saying yes.
        assert_eq!(parse(""), Step::Done(false));
        assert_eq!(parse("something else entirely"), Step::Done(false));
    }

    #[test]
    fn a_message_is_not_a_question() {
        assert_eq!(
            parse("PAM_TEXT_INFO Place your finger"),
            Step::Say("Place your finger".to_string())
        );
        assert_eq!(
            parse("PAM_ERROR_MSG Account locked"),
            Step::Say("Account locked".to_string())
        );
    }
}
