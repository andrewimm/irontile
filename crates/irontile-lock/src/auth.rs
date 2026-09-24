//! Asking PAM whether a password is the user's.
//!
//! PAM is the one part of a lock screen that must not be improvised. Every
//! other mistake makes an ugly screen; a mistake here decides whether the
//! machine opens. So this does what a locker is supposed to do and nothing
//! more, and leaves the policy to the service file in `/etc/pam.d`.

use pam::PamReturnCode as Code;

/// Why an attempt did not succeed.
///
/// Three outcomes rather than one, because they need different words on the
/// glass: a wrong password is the person's problem to fix by typing again, an
/// unusable account is not, and PAM being unreachable is the administrator's.
/// Which module objected belongs in the log, not on a lock screen.
#[derive(Debug, PartialEq)]
pub enum Denied {
    /// The password was not right. The ordinary case, and the only one worth
    /// inviting another attempt for.
    Password,
    /// The password matched but the account may not be used: expired, locked,
    /// or its password must be changed before it will let anyone in.
    Account(Code),
    /// PAM could not be asked at all -- no permission to reach the password
    /// database, a module that would not load. A misconfiguration, not a
    /// failed attempt.
    Unavailable(Code),
    /// There is no service file of that name.
    ///
    /// Its own case because PAM will not tell you: an unknown service falls
    /// through to `/etc/pam.d/other`, which is `pam_deny.so`, which answers
    /// exactly as a wrong password does. A locker that trusted PAM here would
    /// reject the right password forever and say the password was wrong.
    NoService(String),
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Denied::Password => write!(f, "wrong password"),
            Denied::Account(code) => write!(f, "account unavailable ({code:?})"),
            Denied::Unavailable(code) => write!(f, "could not ask PAM ({code:?})"),
            Denied::NoService(name) => {
                write!(
                    f,
                    "no PAM service called {name:?} in /etc/pam.d or /usr/lib/pam.d"
                )
            }
        }
    }
}

/// Sorts a PAM return code into something a lock screen can act on.
///
/// Split out so the mapping can be read and tested without a password, a
/// service file, or a machine to be locked out of.
pub fn classify(code: Code) -> Denied {
    match code {
        // The password did not match, or too many tries have been made. Both
        // mean the same thing to whoever is typing.
        Code::Auth_Err | Code::MaxTries | Code::Cred_Insufficient => Denied::Password,
        // A name nobody could authenticate is not worth distinguishing from a
        // wrong password: saying which it was tells an attacker which accounts
        // exist.
        Code::User_Unknown => Denied::Password,
        Code::Acct_Expired
        | Code::Cred_Expired
        | Code::New_Authtok_Reqd
        | Code::AuthTok_Expired
        | Code::Perm_Denied => Denied::Account(code),
        other => Denied::Unavailable(other),
    }
}

/// Whether `password` authenticates `user` under the named PAM service.
///
/// `service` names a file in `/etc/pam.d`. A locker installs its own, so that
/// what unlocking this screen requires is the administrator's to decide rather
/// than something compiled in. It must define an `account` group as well as an
/// `auth` one -- see `assets/irontile-lock.pam` -- because the check below
/// covers both, and a group a service does not define falls through to
/// `/etc/pam.d/other`, which denies everything.
pub fn verify(service: &str, user: &str, password: &str) -> Result<(), Denied> {
    // Asked before PAM, because PAM cannot answer it. See `NoService`.
    if !service_exists(service) {
        return Err(Denied::NoService(service.to_string()));
    }
    let mut client = pam::Client::with_password(service).map_err(|err| classify(err.0))?;
    client.conversation_mut().set_credentials(user, password);
    // This also runs the account check: the crate calls pam_acct_mgmt after a
    // successful pam_authenticate, so a right password on an expired account
    // still comes back as an error rather than as a way in.
    client.authenticate().map_err(|err| classify(err.0))
}

/// Whether a PAM service of that name is configured.
///
/// Both directories, because a distribution ships its defaults in
/// `/usr/lib/pam.d` and leaves `/etc/pam.d` for the administrator to override
/// them; a file in either is a service that exists.
pub fn service_exists(service: &str) -> bool {
    // A name with a slash in it would reach outside the directory, and no real
    // service has one.
    if service.is_empty() || service.contains('/') {
        return false;
    }
    ["/etc/pam.d", "/usr/lib/pam.d"]
        .iter()
        .any(|dir| std::path::Path::new(dir).join(service).exists())
}

#[cfg(test)]
mod tests {
    use super::{Code, Denied, classify};

    #[test]
    fn a_bad_password_and_an_unknown_user_look_the_same() {
        // Saying which one it was would tell whoever is guessing which accounts
        // exist on the machine.
        assert_eq!(classify(Code::Auth_Err), Denied::Password);
        assert_eq!(classify(Code::User_Unknown), Denied::Password);
        assert_eq!(classify(Code::MaxTries), Denied::Password);
    }

    #[test]
    fn an_account_that_cannot_be_used_is_not_a_wrong_password() {
        // Typing it again will not help, so the screen must not suggest it.
        for code in [
            Code::Acct_Expired,
            Code::New_Authtok_Reqd,
            Code::Perm_Denied,
        ] {
            assert_eq!(classify(code), Denied::Account(code), "{code:?}");
        }
    }

    #[test]
    fn a_service_that_does_not_exist_is_recognised_before_pam_is_asked() {
        use super::service_exists;
        // `login` is on every machine with PAM at all, and is what the
        // lockers here include.
        assert!(service_exists("login"));
        assert!(!service_exists("irontile-definitely-not-a-service"));
        // Nothing that could walk out of the directory counts.
        assert!(!service_exists("../shadow"));
        assert!(!service_exists(""));
    }

    #[test]
    fn a_broken_configuration_is_its_own_answer() {
        // No service file is not a failed attempt, and telling someone their
        // password was wrong when PAM was never asked sends them to fix the
        // one thing that is not broken.
        assert_eq!(
            classify(Code::Open_Err),
            Denied::Unavailable(Code::Open_Err)
        );
        assert_eq!(
            classify(Code::Service_Err),
            Denied::Unavailable(Code::Service_Err)
        );
    }
}
