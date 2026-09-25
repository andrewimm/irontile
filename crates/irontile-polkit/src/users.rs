//! Turning a uid into the name a person recognises.
//!
//! Read from the passwd file rather than through libc, because the whole of
//! what is needed is a name for a number, and the file is where the answer is.
//! A uid that is not in it is not an error: it is shown as a number instead.

/// The login name for a uid, if the passwd file knows one.
pub fn name_of(uid: u32) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    entry_for(&passwd, uid)
}

/// Split out so the parsing can be tested without a passwd file to hand.
fn entry_for(passwd: &str, uid: u32) -> Option<String> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next()?;
        let found: u32 = fields.next()?.parse().ok()?;
        (found == uid).then(|| name.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::entry_for;

    const PASSWD: &str = "root:x:0:0::/root:/bin/bash\n\
                          andrew:x:1000:1000::/home/andrew:/bin/bash\n";

    #[test]
    fn a_uid_becomes_the_name_beside_it() {
        assert_eq!(entry_for(PASSWD, 0).as_deref(), Some("root"));
        assert_eq!(entry_for(PASSWD, 1000).as_deref(), Some("andrew"));
    }

    #[test]
    fn a_uid_nobody_has_is_not_an_error() {
        assert_eq!(entry_for(PASSWD, 4242), None);
    }

    #[test]
    fn a_line_that_makes_no_sense_is_skipped_rather_than_believed() {
        let broken = "nonsense\nandrew:x:1000:1000::/home/andrew:/bin/bash\n";
        assert_eq!(entry_for(broken, 1000).as_deref(), Some("andrew"));
    }
}
