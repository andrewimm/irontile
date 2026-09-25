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
    pub ask: Box<dyn Fn(&Request) -> Answer + Send + Sync>,
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

        let Some((user, secret)) = (self.ask)(&request) else {
            // Dismissed. Not an error: deciding not to is an answer, and the
            // authority reads a return with no response as a refusal.
            return Ok(());
        };

        match crate::helper::authenticate(&user, &cookie, &secret) {
            Ok(true) => Ok(()),
            Ok(false) => Ok(()),
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
