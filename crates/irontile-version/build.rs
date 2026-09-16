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

    // The build machine may hand it over instead of letting git be asked.
    println!("cargo:rerun-if-env-changed=IRONTILE_COMMIT");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");

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
/// Asked of git first, and of the environment when git will not answer. That
/// second case is not hypothetical: a package is built by a different user
/// than the one that made the checkout, in a container, from a repository git
/// may decline to talk about -- and the release built exactly that way came
/// out carrying no commit at all, which is the one build where knowing it
/// matters most.
///
/// Empty when neither can say. A missing hash is better than a wrong one, so
/// nothing is invented.
fn describe() -> String {
    if let Some(given) = from_environment() {
        return given;
    }
    from_git()
}

/// A commit handed over by whatever is doing the building.
///
/// `IRONTILE_COMMIT` for anybody who wants to say it outright, and `GITHUB_SHA`
/// because a workflow sets it already and it names the commit being built even
/// when the checkout is one git will not discuss.
fn from_environment() -> Option<String> {
    let given = std::env::var("IRONTILE_COMMIT")
        .ok()
        .or_else(|| std::env::var("GITHUB_SHA").ok())?;
    let given = given.trim();
    if given.is_empty() {
        return None;
    }
    // Shortened here rather than by whoever set it, so both spellings of the
    // same commit come out looking the same.
    Some(given.chars().take(8).collect())
}

fn from_git() -> String {
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
