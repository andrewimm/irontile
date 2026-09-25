//! Registering with polkit, and answering when it asks.
//!
//! polkit holds the decision; this holds the conversation. When something
//! wants an action the policy says needs a person, the authority calls
//! `BeginAuthentication` here and waits for the reply -- so the reply is not
//! sent until the person has answered or given up.

use std::collections::HashMap;

use zbus::blocking::Connection;
use zbus::zvariant::{OwnedValue, Value};

/// Where the authority lives, and what it is called.
const AUTHORITY: &str = "org.freedesktop.PolicyKit1";
const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const AUTHORITY_INTERFACE: &str = "org.freedesktop.PolicyKit1.Authority";

/// Where this agent puts itself. Nothing else is at this path.
pub const AGENT_PATH: &str = "/org/irontile/PolicyKit1/AuthenticationAgent";

/// A person polkit will accept an answer from.
///
/// An action needing an administrator usually lists root and whoever is in the
/// admin group; one needing only the person at the keyboard lists them alone.
#[derive(Debug, Clone)]
pub struct Identity {
    pub kind: String,
    pub uid: Option<u32>,
}

impl Identity {
    /// The login name, for the helper and for showing.
    ///
    /// A uid with no passwd entry is shown as the number: an agent that
    /// refused to name an identity it could not resolve would be hiding the
    /// one thing the person needs to decide with.
    pub fn name(&self) -> Option<String> {
        // Only a person can be asked for a password. A group is a list of
        // people, and polkit lists both.
        if self.kind != "unix-user" {
            return None;
        }
        let uid = self.uid?;
        Some(crate::users::name_of(uid).unwrap_or_else(|| uid.to_string()))
    }
}

/// What the authority asked for.
#[derive(Debug, Clone)]
pub struct Request {
    pub action_id: String,
    pub message: String,
    pub identities: Vec<Identity>,
}

/// polkit's wire shape for an identity: a kind, and details that depend on it.
type RawIdentity = (String, HashMap<String, OwnedValue>);

/// What the dialog decided: who to authenticate as, and what they typed.
pub type Answer = Option<(String, String)>;

/// How the agent puts the question to somebody: the request, and whether the
/// last go was refused.
pub type Ask = Box<dyn Fn(&Request, bool) -> Answer + Send + Sync>;

/// How many goes somebody gets before the request is refused.
///
/// Three, which is what sudo allows and therefore what a person here already
/// expects. More would be a lock screen's job -- that one waits all night,
/// because the alternative is being shut out of your own machine -- and this
/// is a question something asked on your behalf, which can simply be asked
/// again.
const ATTEMPTS: u32 = 3;

/// How a request ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ending {
    /// The helper said yes, and has told the authority so.
    Authenticated,
    /// Escape, or the dialog went away. Deciding not to is an answer.
    Dismissed,
    /// Every attempt was wrong.
    Exhausted,
}

/// Asks until somebody gets it right, gives up, or runs out of goes.
///
/// The checking is handed in so this can be exercised without a polkit helper
/// on the other end: what is worth testing here is how many times somebody is
/// asked and when the asking stops, neither of which involves a password.
pub fn attempts(
    request: &Request,
    ask: &dyn Fn(&Request, bool) -> Answer,
    mut check: impl FnMut(&str, &str) -> std::io::Result<bool>,
) -> std::io::Result<Ending> {
    let mut refused = false;
    for _ in 0..ATTEMPTS {
        let Some((user, secret)) = ask(request, refused) else {
            return Ok(Ending::Dismissed);
        };
        if check(&user, &secret)? {
            return Ok(Ending::Authenticated);
        }
        // The next dialog says so rather than looking identical to the first,
        // which is the difference between a wrong password and a broken one.
        refused = true;
    }
    Ok(Ending::Exhausted)
}

/// Turns polkit's `a(sa{sv})` into something with names.
pub fn identities_from(raw: &[RawIdentity]) -> Vec<Identity> {
    raw.iter()
        .map(|(kind, details)| Identity {
            kind: kind.clone(),
            uid: details
                .get("uid")
                .and_then(|value| u32::try_from(value).ok()),
        })
        .collect()
}

/// Tells the authority this agent is answering for a session.
///
/// The subject is the session rather than this process: polkit is being told
/// who to ask about anything happening on these displays, and that outlives
/// any one program.
pub fn register(connection: &Connection, session: &str) -> Result<(), String> {
    let mut details: HashMap<&str, Value> = HashMap::new();
    details.insert("session-id", Value::from(session));
    let subject = ("unix-session", details);
    let locale = std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".to_string());

    connection
        .call_method(
            Some(AUTHORITY),
            AUTHORITY_PATH,
            Some(AUTHORITY_INTERFACE),
            "RegisterAuthenticationAgent",
            &(subject, locale.as_str(), AGENT_PATH),
        )
        .map(|_| ())
        .map_err(|err| format!("polkit would not register this agent: {err}"))
}

/// The session this process belongs to, which is what polkit is told about.
pub fn session_id() -> Result<String, String> {
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && !id.is_empty()
    {
        return Ok(id);
    }
    Err("no XDG_SESSION_ID; this has to run inside a login session".to_string())
}

/// The object polkit calls. One authentication at a time, which is what a
/// person can answer anyway.
pub struct Listener {
    /// Asks whoever is at the keyboard, and says what they answered.
    pub ask: Ask,
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl Listener {
    /// Somebody is trying to do something that needs a person to agree.
    ///
    /// This does not return until they have answered or given up, because the
    /// authority is waiting on the reply: returning early would be telling it
    /// the conversation is over while the dialog is still on screen.
    #[allow(clippy::too_many_arguments)]
    fn begin_authentication(
        &self,
        action_id: String,
        message: String,
        _icon_name: String,
        _details: HashMap<String, String>,
        cookie: String,
        identities: Vec<RawIdentity>,
    ) -> zbus::fdo::Result<()> {
        let request = Request {
            action_id,
            message,
            identities: identities_from(&identities),
        };

        // Every ending is a plain return: the helper tells the authority
        // whether anybody got in, so there is nothing to report here but that
        // the conversation is over.
        match attempts(&request, self.ask.as_ref(), |user, secret| {
            crate::helper::authenticate(user, &cookie, secret)
        }) {
            Ok(_) => Ok(()),
            Err(err) => Err(zbus::fdo::Error::Failed(format!(
                "could not reach polkit's helper: {err}"
            ))),
        }
    }

    /// Whoever asked has stopped waiting.
    fn cancel_authentication(&self, _cookie: String) -> zbus::fdo::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ATTEMPTS, Answer, Ending, Identity, Request, attempts};
    use std::cell::RefCell;

    fn request() -> Request {
        Request {
            action_id: "org.freedesktop.systemd1.manage-units".to_string(),
            message: "Authentication is required".to_string(),
            identities: vec![Identity {
                kind: "unix-user".to_string(),
                uid: Some(1000),
            }],
        }
    }

    #[test]
    fn a_right_password_is_asked_for_once() {
        let asked = RefCell::new(0);
        let ask = |_: &Request, _: bool| -> Answer {
            *asked.borrow_mut() += 1;
            Some(("andrew".to_string(), "right".to_string()))
        };
        let ending = attempts(&request(), &ask, |_, secret| Ok(secret == "right")).unwrap();
        assert_eq!(ending, Ending::Authenticated);
        assert_eq!(*asked.borrow(), 1);
    }

    #[test]
    fn three_wrong_passwords_and_no_more() {
        // Three, like sudo. A fourth dialog would be this agent deciding the
        // machine's policy for it.
        let asked = RefCell::new(0);
        let ask = |_: &Request, _: bool| -> Answer {
            *asked.borrow_mut() += 1;
            Some(("andrew".to_string(), "wrong".to_string()))
        };
        let ending = attempts(&request(), &ask, |_, _| Ok(false)).unwrap();
        assert_eq!(ending, Ending::Exhausted);
        assert_eq!(*asked.borrow(), ATTEMPTS as i32);
    }

    #[test]
    fn the_second_dialog_says_the_first_was_refused() {
        // Otherwise a wrong password looks exactly like a dialog that ignored
        // you, and the natural response is to type the same thing again.
        let told = RefCell::new(Vec::new());
        let ask = |_: &Request, refused: bool| -> Answer {
            told.borrow_mut().push(refused);
            Some(("andrew".to_string(), "wrong".to_string()))
        };
        let _ = attempts(&request(), &ask, |_, _| Ok(false)).unwrap();
        assert_eq!(*told.borrow(), vec![false, true, true]);
    }

    #[test]
    fn giving_up_stops_the_asking() {
        let asked = RefCell::new(0);
        let ask = |_: &Request, _: bool| -> Answer {
            *asked.borrow_mut() += 1;
            None
        };
        let ending = attempts(&request(), &ask, |_, _| Ok(true)).unwrap();
        assert_eq!(ending, Ending::Dismissed);
        assert_eq!(*asked.borrow(), 1, "asked again after being told no");
    }

    #[test]
    fn a_helper_that_cannot_be_reached_is_not_a_wrong_password() {
        // The difference matters: one is somebody mistyping, the other is a
        // machine that cannot authenticate anybody, and answering the second
        // with "try again" would ask three times for nothing.
        let ask =
            |_: &Request, _: bool| -> Answer { Some(("andrew".to_string(), "x".to_string())) };
        let broken = attempts(&request(), &ask, |_, _| {
            Err(std::io::Error::other("no helper"))
        });
        assert!(broken.is_err());
    }
}
