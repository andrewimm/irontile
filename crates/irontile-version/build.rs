//! Bakes the commit being built into the binaries.
//!
//! A version number says which release something came from, which stops being
//! enough the moment anybody builds between releases: a package built from the
//! working tree carries the same `0.2.6` as the release of that name and is not
//! the same software. The commit is what tells them apart, and "is the machine
//! running what I think it is" is a question worth being able to answer without
//! guessing.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Rebuilt when the checked-out commit moves. HEAD covers switching branch;
    // the file HEAD points at covers committing on the one you are on, which is
    // the case that would otherwise bake in a hash that quietly goes stale.
    if let Some(git) = git_dir() {
        println!("cargo:rerun-if-changed={git}/HEAD");
        if let Ok(head) = std::fs::read_to_string(format!("{git}/HEAD"))
            && let Some(reference) = head.strip_prefix("ref: ")
        {
            println!("cargo:rerun-if-changed={git}/{}", reference.trim());
        }
    }

    println!("cargo:rustc-env=IRONTILE_COMMIT={}", describe());
}

fn git_dir() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The short commit, with `-dirty` when the tree has uncommitted changes.
///
/// Empty when there is no git to ask -- a source tarball, or a build somewhere
/// the repository did not come along. A missing hash is better than a wrong
/// one, so nothing is invented.
fn describe() -> String {
    let Some(out) = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
    else {
        return String::new();
    };
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if commit.is_empty() {
        return commit;
    }

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|out| !out.stdout.is_empty())
        .unwrap_or(false);
    if dirty {
        format!("{commit}-dirty")
    } else {
        commit
    }
}
