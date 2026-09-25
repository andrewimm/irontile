//! Task runner for the irontile workspace.
//!
//! Invoked as `cargo xtask <subcommand>` via the alias in `.cargo/config.toml`.
//! Deliberately dependency-free: argument parsing is hand-rolled so the build
//! graph for `cargo xtask` stays as small as the workspace itself.

mod multihead;

use std::env;
use std::process::{Command, ExitCode};

const HELP: &str = "\
cargo xtask - irontile task runner

USAGE:
    cargo xtask <SUBCOMMAND> [OPTIONS] [-- <ARGS>...]

SUBCOMMANDS:
    run     Build and launch the compositor. Everything after `--` is forwarded
            to the compositor binary.
    test    Verify the workspace: rustfmt check, clippy (warnings denied), then
            the full test suite.
    multihead
            Exercise multi-display behaviour against a running compositor over
            its control socket, writing a log. Started by try-multihead.sh as a
            startup command, so it runs inside the session under test.
    release-check <TAG>
            Check that TAG names the version the workspace would actually
            build, before a release is cut from it.
    aur [TAG]
            Fill in the AUR recipe for a tag that has been pushed, writing the
            PKGBUILD and .SRCINFO to publish into target/aur and printing how
            to publish them. Defaults to the workspace version. Needs an Arch
            machine, for makepkg.

OPTIONS (test):
    --skip-lints    Run only the test suite, skipping rustfmt and clippy.
    --skip-tests    Run only rustfmt and clippy.

OPTIONS (run):
    --release       Build with optimizations.
";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let (subcommand, rest) = match args.split_first() {
        Some((first, rest)) => (first.as_str(), rest),
        None => {
            print!("{HELP}");
            return Err("missing subcommand".into());
        }
    };

    match subcommand {
        "run" => cmd_run(rest),
        "test" => cmd_test(rest),
        "multihead" => multihead::run(rest),
        "release-check" => cmd_release_check(rest),
        "aur" => cmd_aur(rest),
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        other => {
            print!("{HELP}");
            Err(format!("unknown subcommand `{other}`"))
        }
    }
}

/// Checks a release tag against the version the workspace would build.
///
/// A package takes its version from `Cargo.toml` and never from the tag it was
/// built at, so the two can disagree silently: a `v0.2.0` tag happily produces
/// packages called 0.1.0, and the mistake is only visible once somebody
/// installs one.
fn cmd_release_check(args: &[String]) -> Result<(), String> {
    let tag = args
        .first()
        .ok_or("release-check needs the tag to check, such as v0.1.0")?;
    let version = workspace_version()?;
    let expected = format!("v{version}");
    if *tag != expected {
        return Err(format!(
            "tag {tag} would build version {version}, which is released as \
             {expected}; set one to match the other"
        ));
    }

    // The Arch recipe carries the version a second time, and it is fetched from
    // a release tarball rather than from the tree, so a stale one here builds
    // the previous release under the new tag's name without anything failing.
    if let Some(pkgver) = pkgbuild_version()?
        && pkgver != version
    {
        return Err(format!(
            "packaging/PKGBUILD still says pkgver={pkgver}, but this release is \
             {version}; run `updpkgsums` there after changing it"
        ));
    }

    println!("{tag} matches the workspace version");
    Ok(())
}

/// The `pkgver` from the Arch recipe, if the repository carries one.
fn pkgbuild_version() -> Result<Option<String>, String> {
    let path = repo_root()?.join("packaging").join("PKGBUILD");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("pkgver=") {
            return Ok(Some(rest.trim().to_string()));
        }
    }
    Err("no pkgver in packaging/PKGBUILD".to_string())
}

fn repo_root() -> Result<std::path::PathBuf, String> {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "xtask is not inside a workspace".to_string())
}

/// The `version` under `[workspace.package]`.
///
/// Read by hand rather than through a TOML parser, so that xtask keeps building
/// with nothing behind it. The shape it needs is two lines of a file that lives
/// next door and changes about once a release.
fn workspace_version() -> Result<String, String> {
    let path = repo_root()?.join("Cargo.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;

    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[workspace.package]";
            continue;
        }
        if in_package
            && let Some(rest) = line.strip_prefix("version")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            return Ok(value.trim().trim_matches('"').to_string());
        }
    }
    Err("no version under [workspace.package]".to_string())
}

fn cmd_run(args: &[String]) -> Result<(), String> {
    // Everything after a bare `--` belongs to the compositor, not to us.
    let (ours, theirs) = split_forwarded(args);

    let mut cargo = cargo();
    cargo.args(["run", "--package", "irontile-comp"]);
    for flag in ours {
        match flag.as_str() {
            "--release" => {
                cargo.arg("--release");
            }
            other => return Err(format!("unknown flag for `run`: {other}")),
        }
    }
    if !theirs.is_empty() {
        cargo.arg("--");
        cargo.args(theirs);
    }
    exec(cargo)
}

fn cmd_test(args: &[String]) -> Result<(), String> {
    let mut lints = true;
    let mut tests = true;
    for flag in args {
        match flag.as_str() {
            "--skip-lints" => lints = false,
            "--skip-tests" => tests = false,
            other => return Err(format!("unknown flag for `test`: {other}")),
        }
    }

    if lints {
        let mut fmt = cargo();
        fmt.args(["fmt", "--all", "--check"]);
        exec(fmt)?;

        let mut clippy = cargo();
        clippy.args([
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ]);
        exec(clippy)?;
    }

    if tests {
        let mut test = cargo();
        test.args(["test", "--workspace", "--all-features"]);
        exec(test)?;
    }

    Ok(())
}

/// Splits `args` at the first bare `--`, returning (before, after).
fn split_forwarded(args: &[String]) -> (&[String], &[String]) {
    match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[]),
    }
}

fn cargo() -> Command {
    let mut cmd = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(workspace_root());
    cmd
}

fn workspace_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR is `<root>/xtask`.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest always has a parent")
        .to_path_buf()
}

fn exec(mut cmd: Command) -> Result<(), String> {
    eprintln!("+ {}", render(&cmd));
    let status = cmd
        .status()
        .map_err(|e| format!("failed to spawn `{}`: {e}", render(&cmd)))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{}` exited with {status}", render(&cmd)))
    }
}

fn render(cmd: &Command) -> String {
    let mut out = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        out.push(' ');
        out.push_str(&arg.to_string_lossy());
    }
    out
}

/// Fills the AUR template in for a tag that exists, and says how to publish it.
///
/// Writing the checksum is the whole reason this exists and the reason it
/// cannot be done any earlier: a tarball's checksum cannot be known before the
/// tarball, and GitHub makes the tarball from the tag. So the template is what
/// the repository carries and the pair to publish is generated, rather than a
/// checksum being committed for a file that does not exist yet.
///
/// Nothing here can publish. Pushing to the AUR needs an account and an SSH key
/// that belongs to a person rather than to a repository, so the last step is
/// printed and left to be run by hand.
fn cmd_aur(args: &[String]) -> Result<(), String> {
    let version = match args.first() {
        Some(tag) => tag.trim_start_matches('v').to_string(),
        None => workspace_version()?,
    };
    let root = repo_root()?;
    let template_path = root.join("packaging").join("aur").join("PKGBUILD.in");
    let template = std::fs::read_to_string(&template_path)
        .map_err(|err| format!("failed to read {}: {err}", template_path.display()))?;

    let out = root.join("target").join("aur");
    std::fs::create_dir_all(&out)
        .map_err(|err| format!("failed to make {}: {err}", out.display()))?;

    let url = format!("https://github.com/andrewimm/irontile/archive/refs/tags/v{version}.tar.gz");
    let tarball = out.join(format!("irontile-{version}.tar.gz"));
    // -f so that a 404 is a failure rather than a saved error page, which is
    // the shape this goes wrong in: asking for a tag nobody pushed.
    let mut curl = Command::new("curl");
    curl.args(["-fsSL", "-o"]).arg(&tarball).arg(&url);
    exec(curl)
        .map_err(|err| format!("{err}\nIs v{version} pushed? The tarball is made from the tag."))?;

    let sum = sha256_of(&tarball)?;
    let pkgbuild = template
        .replace("@PKGVER@", &version)
        .replace("@SHA256@", &sum);
    if pkgbuild.contains('@') && pkgbuild.contains("@PKGVER@") {
        return Err("the template still has placeholders after filling it in".into());
    }
    let pkgbuild_path = out.join("PKGBUILD");
    std::fs::write(&pkgbuild_path, &pkgbuild)
        .map_err(|err| format!("failed to write {}: {err}", pkgbuild_path.display()))?;

    // .SRCINFO is what the AUR actually reads; a package whose .SRCINFO does
    // not match its PKGBUILD is the classic way to publish a version nobody
    // can see. makepkg writes it so that it is written the way makepkg would.
    let mut srcinfo = Command::new("makepkg");
    srcinfo.arg("--printsrcinfo").current_dir(&out);
    let printed = srcinfo
        .output()
        .map_err(|err| format!("failed to run makepkg: {err}\nThis step needs an Arch machine."))?;
    if !printed.status.success() {
        return Err(format!(
            "makepkg --printsrcinfo failed: {}",
            String::from_utf8_lossy(&printed.stderr).trim()
        ));
    }
    let srcinfo_path = out.join(".SRCINFO");
    std::fs::write(&srcinfo_path, &printed.stdout)
        .map_err(|err| format!("failed to write {}: {err}", srcinfo_path.display()))?;

    println!("wrote {} and .SRCINFO", pkgbuild_path.display());
    println!("  pkgver  {version}");
    println!("  sha256  {sum}");
    println!();
    println!("To publish, from a machine whose SSH key is on your AUR account:");
    println!();
    println!("    git clone ssh://aur@aur.archlinux.org/irontile.git /tmp/aur-irontile");
    println!("    cp {} /tmp/aur-irontile/", pkgbuild_path.display());
    println!("    cp {} /tmp/aur-irontile/", srcinfo_path.display());
    println!("    cd /tmp/aur-irontile");
    println!("    git add PKGBUILD .SRCINFO");
    println!("    git commit -m 'irontile {version}'");
    println!("    git push origin HEAD:master");
    println!();
    // Both of these bite exactly once, on the first publish, and the first
    // publish is the one nobody has done before.
    println!("The first push creates the package: cloning a name nobody has taken");
    println!("gives an empty repository, which is why the files are added rather");
    println!("than committed with -a, and pushed to master by name.");
    Ok(())
}

/// The sha256 of a file, as `sha256sum` prints it.
///
/// Shelled out rather than added as a dependency: xtask has none, and this is
/// the one place in the workspace that wants a hash.
fn sha256_of(path: &std::path::Path) -> Result<String, String> {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|err| format!("failed to run sha256sum: {err}"))?;
    if !out.status.success() {
        return Err(format!("sha256sum failed on {}", path.display()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "sha256sum printed nothing".to_string())
}

#[cfg(test)]
mod recipes {
    const TREE: &str = include_str!("../../packaging/PKGBUILD");
    const AUR: &str = include_str!("../../packaging/aur/PKGBUILD.in");

    /// The entries of a `name=(...)` array, however many lines it runs to.
    fn array(text: &str, name: &str) -> Vec<String> {
        let start = format!("{name}=(");
        let mut body = String::new();
        let mut inside = false;
        for line in text.lines() {
            if !inside && line.starts_with(&start) {
                inside = true;
                body.push_str(&line[start.len()..]);
            } else if inside {
                body.push(' ');
                body.push_str(line);
            }
            if inside && line.trim_end().ends_with(')') {
                break;
            }
        }
        body.split('\'')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    }

    /// Everything `package()` puts on disk, as written.
    fn installs(text: &str) -> Vec<String> {
        text.lines()
            .map(str::trim)
            .filter(|line| line.starts_with("install -Dm") || line.starts_with("for page in"))
            .map(str::to_owned)
            .collect()
    }

    /// The two recipes ask for the same things.
    ///
    /// They exist separately because one packages this checkout and the other
    /// fetches a release tarball, but what a package needs does not depend on
    /// where its source came from. The drift is a dependency added to the one
    /// that gets built here and missing from the one people install.
    #[test]
    fn the_two_recipes_ask_for_the_same_things() {
        for name in [
            "depends",
            "optdepends",
            "makedepends",
            "checkdepends",
            "backup",
        ] {
            let tree = array(TREE, name);
            assert!(
                !tree.is_empty(),
                "{name} was not found in packaging/PKGBUILD"
            );
            assert_eq!(tree, array(AUR, name), "{name} differs between the recipes");
        }
    }

    /// And install the same files.
    #[test]
    fn the_two_recipes_install_the_same_files() {
        let tree = installs(TREE);
        assert!(tree.len() > 5, "only {} install lines found", tree.len());
        assert_eq!(tree, installs(AUR), "the recipes package different files");
    }

    /// The template is still a template.
    #[test]
    fn the_template_has_somewhere_to_put_the_version_and_the_checksum() {
        assert!(AUR.contains("pkgver=@PKGVER@"), "no version placeholder");
        assert!(
            AUR.contains("sha256sums=('@SHA256@')"),
            "no checksum placeholder"
        );
    }
}
