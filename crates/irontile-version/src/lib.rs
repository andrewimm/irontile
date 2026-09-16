//! What build this is.
//!
//! Every binary answers `--version`, and a version alone answers the wrong
//! question: it says which release the number came from, not whether the thing
//! running is that release. Between a package built from a working tree and the
//! release of the same name there is no difference in the number and every
//! difference in the code.

/// The commit this was built from, empty when there was no git to ask.
const COMMIT: &str = env!("IRONTILE_COMMIT");

/// The commit this was built from, if it is known.
pub fn commit() -> Option<&'static str> {
    (!COMMIT.is_empty()).then_some(COMMIT)
}

/// One line naming a program, its version, and the commit behind it.
///
/// `irontile 0.2.6 (e8e57c2f)`, or `irontile 0.2.6` where the commit is not
/// known, or `irontile 0.2.6 (e8e57c2f-dirty)` when it was built from a tree
/// with changes in it -- which is worth saying out loud, because a dirty build
/// is one nobody else can reproduce.
pub fn line(program: &str, version: &str) -> String {
    describe(program, version, commit())
}

/// The same line, told what the commit is rather than looking it up.
///
/// Split out so the shape can be tested without the test depending on how the
/// machine running it was checked out. A build from a tarball has no commit and
/// is not a broken build, so a test that demands one is a test that fails on
/// the machines least able to explain why.
fn describe(program: &str, version: &str, commit: Option<&str>) -> String {
    match commit {
        Some(commit) => format!("{program} {version} ({commit})"),
        None => format!("{program} {version}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{describe, line};

    #[test]
    fn a_commit_is_shown_in_brackets() {
        assert_eq!(
            describe("irontile", "0.2.7", Some("468dceb0")),
            "irontile 0.2.7 (468dceb0)"
        );
    }

    #[test]
    fn a_build_with_no_commit_still_names_itself() {
        // A source tarball, or anywhere else git could not be asked. Saying
        // less is right; saying nothing, or refusing to build, is not.
        assert_eq!(describe("irontile", "0.2.7", None), "irontile 0.2.7");
    }

    #[test]
    fn a_dirty_tree_says_so_where_it_can_be_seen() {
        let text = describe("irontile", "0.2.7", Some("468dceb0-dirty"));
        assert!(text.ends_with("-dirty)"), "{text}");
    }

    #[test]
    fn the_real_line_names_the_program_and_its_version() {
        // Whatever this machine turned out to have: the version is always
        // there, and the commit is not this test's business.
        assert!(line("irontile", "0.2.7").starts_with("irontile 0.2.7"));
    }
}
