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
    match commit() {
        Some(commit) => format!("{program} {version} ({commit})"),
        None => format!("{program} {version}"),
    }
}

#[cfg(test)]
mod tests {
    use super::line;

    #[test]
    fn the_line_names_the_program_and_its_version() {
        let text = line("irontile", "0.2.6");
        assert!(text.starts_with("irontile 0.2.6"), "{text}");
    }

    #[test]
    fn a_commit_is_shown_in_brackets_when_there_is_one() {
        // This test runs from a checkout, so there is one.
        let text = line("irontile", "0.2.6");
        assert!(
            text.contains('(') && text.ends_with(')'),
            "built from a checkout but said nothing about the commit: {text}"
        );
    }
}
