//! `lyracore packages add <local-folder>` and `lyracore packages list`.
//!
//! A Package is a drop-in folder under `packages/<name>/` that the server build compiles into the
//! module with no core-file edits: `module/build.rs` discovers it, generates `pub mod pkg_<name>`
//! for its `src/mod.rs`, and registers every marker it finds. `importer --pack-client` picks up its
//! `client/` half the same way. Installing one is therefore not a configuration change — it is
//! adding trusted code to the realm — so `add` shows a deterministic inventory of what it registers
//! and asks before it copies anything.
//!
//! COPY, NEVER SYMLINK. A symlinked Package would compile from a folder outside the checkout, so
//! `preflight`, `publish` and `client sync` would each read whatever that folder happened to say at
//! the time. The copy is the installed Package; the folder it came from is only its Package Source.
//! `packages list` reports the two drifting apart.
//!
//! WHERE PACKAGES LIVE: enabled ones in `packages/` (what the build reads), disabled ones in
//! `.lyracore/packages-disabled/` (git-ignored local state the build cannot see). Enabling and
//! disabling are separate issues; `add` and `list` only have to know that BOTH are inventories a
//! new name must not collide with — an installed name that reappears when a Package is re-enabled
//! is a collision the operator would meet much later, holding two folders and no way to tell which
//! one the build compiled.

pub mod review;
pub mod stamp;

use crate::cmd::import::Prompt;
use crate::cmd::preflight;
use crate::proc::ProcessRunner;
use crate::project::ProjectLayout;
use crate::{Error, Result};
use review::TrustReview;
use stamp::ProvenanceStamp;
use std::path::{Path, PathBuf};

/// A Package folder name the server build will accept.
///
/// The rule is `module/build.rs::package_ident()`, mirrored exactly rather than approximated: the
/// build maps `my-package` onto the Rust module `pkg_my_package` and PANICS on a name that does not
/// map cleanly. A name this CLI accepted and that build refused would be an install that only fails
/// at the next `preflight`, with a build-script panic naming a folder the operator did not create
/// by hand.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageName(String);

impl PackageName {
    pub fn parse(raw: &str) -> Result<Self> {
        let valid = !raw.is_empty()
            && raw.starts_with(|c: char| c.is_ascii_alphabetic())
            && raw
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid {
            return Err(Error::Usage(format!(
                "'{raw}' is not a usable Package name. A Package folder becomes the Rust module \
                 `pkg_<name>` in the module wasm, so the name must start with a letter and hold \
                 only letters, digits, '_' and '-' (the server build panics on anything else)."
            )));
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The Rust module suffix the build derives: hyphens fold to underscores. TWO names that fold
    /// to the same identifier cannot be installed together — the generated `pkg_<ident>` modules
    /// would collide — which is why collision checks compare this and not the folder name.
    pub fn rust_ident(&self) -> String {
        self.0.replace('-', "_")
    }
}

/// Which inventory a Package is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageState {
    /// Under `packages/` — compiled into the module by the next `preflight`/`publish`.
    Enabled,
    /// Under `.lyracore/packages-disabled/` — kept, but invisible to the build.
    Disabled,
}

impl PackageState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PackageState::Enabled => "enabled",
            PackageState::Disabled => "disabled",
        }
    }
}

/// One Package on disk, from either inventory.
#[derive(Debug, Clone)]
pub struct InstalledPackage {
    pub name: PackageName,
    pub state: PackageState,
    pub dir: PathBuf,
    /// `None` for a Package created by hand or one that predates `packages add`.
    pub stamp: Option<ProvenanceStamp>,
}

/// Every installed Package, enabled then disabled, each set sorted by name.
///
/// A folder whose name the build would refuse is still listed — it exists, and refusing to mention
/// it would hide the one thing that explains the build failure it causes. Its name is reported
/// verbatim.
pub fn inventory(project: &ProjectLayout) -> Result<Vec<InstalledPackage>> {
    let mut installed = Vec::new();
    for (state, root) in [
        (PackageState::Enabled, project.packages_dir()),
        (PackageState::Disabled, project.packages_disabled_dir()),
    ] {
        if !root.is_dir() {
            continue;
        }
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let raw = dir.file_name().unwrap_or_default().to_string_lossy();
            installed.push(InstalledPackage {
                name: PackageName::parse(&raw).unwrap_or(PackageName(raw.into_owned())),
                state,
                stamp: ProvenanceStamp::read(&dir),
                dir,
            });
        }
    }
    Ok(installed)
}

/// Refuse a name either inventory already holds, comparing the Rust identifiers the build derives
/// rather than the folder names.
pub fn check_collision(project: &ProjectLayout, name: &PackageName) -> Result<()> {
    let ident = name.rust_ident();
    for existing in inventory(project)? {
        if existing.name.rust_ident() != ident {
            continue;
        }
        let same_spelling = existing.name == *name;
        let why = if same_spelling {
            format!(
                "a {} Package is already called '{}'",
                existing.state.as_str(),
                existing.name.as_str()
            )
        } else {
            format!(
                "the {} Package '{}' already folds onto the same module `pkg_{ident}` ('-' and \
                 '_' are the same character to the build)",
                existing.state.as_str(),
                existing.name.as_str()
            )
        };
        return Err(Error::Usage(format!(
            "cannot install '{}': {why}. It is at {}. Nothing was copied.",
            name.as_str(),
            existing.dir.display()
        )));
    }
    Ok(())
}

/// The shapes `module/build.rs` accepts.
///
/// A `client/` with no `src/` is a legal client-only Package. A `src/` without `src/mod.rs` is not:
/// the build panics rather than silently skipping it, so this refuses it here where the operator
/// still has the folder in front of them. Neither directory means the folder is not a Package at
/// all — the shape the build only warns about, because by then it is too late to ask.
pub fn validate_shape(source: &Path) -> Result<()> {
    let has_src = source.join("src").is_dir();
    let has_client = source.join("client").is_dir();
    if !has_src && !has_client {
        return Err(Error::Usage(format!(
            "{} is not a Package: it has neither src/ (Rust compiled into the module) nor client/ \
             (addons and client overrides). One of the two is required.",
            source.display()
        )));
    }
    if has_src && !source.join("src").join("mod.rs").is_file() {
        return Err(Error::Usage(format!(
            "{}/src/ has no mod.rs. A Package's Rust root must be src/mod.rs — the server build \
             refuses any other spelling rather than skipping the Package silently.",
            source.display()
        )));
    }
    Ok(())
}

// =============================================================================================
//  `packages add`
// =============================================================================================

/// Install a local folder as an enabled Package.
///
/// Order matters and is the whole design: everything that can refuse the install does so BEFORE
/// anything is copied, the operator sees the trust review before they are asked, and preflight runs
/// after the copy because it is the first check that can see the Package compiled in. Publishing is
/// never part of this command — the module on the node is unchanged until the operator runs
/// `lyracore publish` themselves.
pub fn add(
    project: &ProjectLayout,
    runner: &dyn ProcessRunner,
    prompt: &dyn Prompt,
    source: &str,
    yes: bool,
) -> Result<()> {
    let source = std::fs::canonicalize(source).map_err(|e| {
        Error::Usage(format!(
            "cannot read the folder to install: {source} ({e}). `packages add` takes a path to a \
             Package folder on this machine."
        ))
    })?;
    if !source.is_dir() {
        return Err(Error::Usage(format!(
            "{} is a file. `packages add` takes the Package FOLDER — the one holding src/ and/or \
             client/.",
            source.display()
        )));
    }

    let name = PackageName::parse(&source.file_name().unwrap_or_default().to_string_lossy())?;
    validate_shape(&source)?;

    let destination = project.packages_dir().join(name.as_str());
    if source.starts_with(project.packages_dir())
        || source.starts_with(project.packages_disabled_dir())
    {
        return Err(Error::Usage(format!(
            "{} is already inside this checkout's Package inventory. `packages add` installs a \
             folder from elsewhere on this machine.",
            source.display()
        )));
    }
    check_collision(project, &name)?;

    let review = TrustReview::scan(&source)?;
    println!();
    print!("{}", review.render(&source));
    println!();

    confirm(prompt, &name, &destination, yes)?;

    copy_tree(&source, &destination)?;
    let identity = stamp::content_identity(&destination)?;
    ProvenanceStamp::local(&source, identity.clone(), stamp::now_unix()).write(&destination)?;
    println!();
    println!("installed {} -> {}", name.as_str(), destination.display());
    println!("  source    local {}", source.display());
    println!("  identity  {identity}");

    println!();
    println!("running preflight with the Package compiled in");
    preflight::run(project, runner).map_err(|e| {
        Error::Process(format!(
            "preflight failed with '{}' installed, so it has NOT been published and the module on \
             the node is unchanged.\n  The Package is on disk at {}. Fix it there and re-run \
             `lyracore preflight`, or undo the install with:\n      rm -rf {}\n  ({e})",
            name.as_str(),
            destination.display(),
            destination.display()
        ))
    })?;

    println!();
    println!("'{}' is installed and preflight is green. Two steps remain, and this command runs neither:", name.as_str());
    println!("  lyracore publish       compile the Package into the module and publish it to every database");
    if review.addons.is_empty() && review.client_overrides == 0 {
        println!("  lyracore client sync   not needed: this Package ships no client content");
    } else {
        println!(
            "  lyracore client sync   install its {} addon(s) and {} client override(s) into your client",
            review.addons.len(),
            review.client_overrides
        );
    }
    Ok(())
}

fn confirm(prompt: &dyn Prompt, name: &PackageName, destination: &Path, yes: bool) -> Result<()> {
    if yes {
        println!("Install confirmed on the command line (--yes).");
        return Ok(());
    }
    let answer = prompt.ask(&format!(
        "Install '{}' into {}? Type 'yes' to continue: ",
        name.as_str(),
        destination.display()
    ))?;
    if !answer.eq_ignore_ascii_case("yes") {
        return Err(Error::Usage(format!(
            "not installing: the answer was {answer:?}, and only 'yes' is consent. Nothing was \
             copied."
        )));
    }
    Ok(())
}

/// Copy a Package tree file by file.
///
/// Two things are refused rather than resolved. A SYMLINK anywhere in the tree would either be
/// followed (smuggling content from outside the folder the operator reviewed) or recreated (an
/// install that stops working when its target moves); both contradict "the copy is the installed
/// Package". A `.git` directory is skipped, because a Package Source is often a checkout of its own
/// and a nested repository inside `packages/` is not something the operator asked for.
fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        if entry.file_type()?.is_symlink() {
            return Err(Error::Usage(format!(
                "{} is a symlink. A Package is copied, never linked, so every file has to be one \
                 this folder actually holds. Replace the link with its content and re-run.",
                path.display()
            )));
        }
        let target = destination.join(&name);
        if path.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

// =============================================================================================
//  `packages list`
// =============================================================================================

/// Report every installed Package: which inventory it is in, where it came from, what its content
/// was when it was installed, whether it still matches, and what it registers.
pub fn list(project: &ProjectLayout) -> Result<()> {
    let installed = inventory(project)?;
    if installed.is_empty() {
        println!("no Packages installed.");
        println!(
            "`lyracore packages add <folder>` installs one from a folder on this machine, into {}.",
            project.packages_dir().display()
        );
        return Ok(());
    }

    let enabled = installed
        .iter()
        .filter(|p| p.state == PackageState::Enabled)
        .count();
    println!(
        "{} Package(s): {enabled} enabled, {} disabled",
        installed.len(),
        installed.len() - enabled
    );

    for package in &installed {
        let identity = stamp::content_identity(&package.dir)?;
        println!();
        println!("{}  {}", package.name.as_str(), package.state.as_str());
        match &package.stamp {
            Some(recorded) => {
                println!(
                    "  source    {} {}",
                    blank_as_unrecorded(&recorded.source_kind),
                    blank_as_unrecorded(&recorded.source)
                );
                println!(
                    "  installed {}",
                    blank_as_unrecorded(&recorded.installed_at)
                );
                println!("  identity  {identity}  {}", drift(recorded, &identity));
            }
            None => {
                // Created by hand or installed before `packages add` existed. It is a real
                // Package the build compiles; only its provenance is unknown.
                println!("  source    (unrecorded — no provenance stamp: created by hand, or predates `packages add`)");
                println!("  identity  {identity}  (nothing recorded to compare against)");
            }
        }
        println!(
            "  content   {}",
            TrustReview::scan(&package.dir)?.kinds_summary()
        );
    }
    Ok(())
}

fn drift(recorded: &ProvenanceStamp, current: &str) -> &'static str {
    if recorded.content_identity.is_empty() {
        "(the stamp records no identity to compare against)"
    } else if recorded.content_identity == current {
        "clean"
    } else {
        "LOCALLY DRIFTED — the installed copy no longer matches what was installed"
    }
}

fn blank_as_unrecorded(value: &str) -> &str {
    if value.is_empty() {
        "(unrecorded)"
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::fake::{FakeStack, FAKE_RUST_VERSION, FAKE_SPACETIME_VERSION};
    use tempfile::TempDir;

    /// A checkout `preflight` passes in: the same fixture its own tests use, so an `add` test
    /// exercises the real post-install gate rather than a stubbed one.
    fn checkout(tmp: &TempDir) -> ProjectLayout {
        let root = tmp.path().join("checkout");
        std::fs::create_dir_all(root.join("module/src")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(
            root.join(ProjectLayout::RUST_TOOLCHAIN),
            format!("[toolchain]\nchannel = \"{FAKE_RUST_VERSION}\"\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("module/Cargo.toml"),
            format!(
                "[dependencies]\nspacetimedb = {{ version = \"={FAKE_SPACETIME_VERSION}\" }}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("module/src/lib.rs"),
            "#[client_visibility_filter]\nconst CHARACTER_RLS: Filter =\n    \
             Filter::Sql(\"SELECT * FROM game_character WHERE owner_identity = :sender\");\n",
        )
        .unwrap();
        ProjectLayout::from_root(&root).unwrap()
    }

    /// A valid Rust Package folder outside the checkout.
    fn candidate(tmp: &TempDir, name: &str) -> PathBuf {
        let dir = tmp.path().join("sources").join(name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/mod.rs"),
            "#[spacetimedb::table(name = pkg_greeter_log)]\npub struct Log { pub id: u64 }\n\
             game_hook!(on_login, fn greet(ctx, payload) { });\n",
        )
        .unwrap();
        dir
    }

    struct Answer(&'static str);
    impl Prompt for Answer {
        fn ask(&self, _question: &str) -> Result<String> {
            Ok(self.0.to_string())
        }
    }

    // ---- the name contract, mirrored from module/build.rs ----

    #[test]
    fn a_name_is_usable_exactly_when_the_server_build_would_accept_it() {
        // The rule is build.rs's `package_ident`: [a-zA-Z][a-zA-Z0-9_-]*. A name this CLI took and
        // that build panicked on would be an install that breaks the next preflight.
        for ok in ["greeter", "my-package", "my_package", "a", "pkg2"] {
            assert!(PackageName::parse(ok).is_ok(), "{ok}");
        }
        for refused in [
            "",
            "2fast",
            "-leading",
            "_leading",
            "has space",
            "dot.name",
            "üml",
        ] {
            let error = PackageName::parse(refused).unwrap_err();
            assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{refused}");
        }
    }

    #[test]
    fn hyphens_and_underscores_fold_onto_the_same_module() {
        assert_eq!(
            PackageName::parse("my-package").unwrap().rust_ident(),
            "my_package"
        );
        assert_eq!(
            PackageName::parse("my_package").unwrap().rust_ident(),
            "my_package"
        );
    }

    // ---- the shape contract ----

    #[test]
    fn a_client_only_package_is_valid_and_a_src_without_mod_rs_is_not() {
        let tmp = TempDir::new().unwrap();

        let client_only = tmp.path().join("ui");
        std::fs::create_dir_all(client_only.join("client/addons/UI")).unwrap();
        validate_shape(&client_only).unwrap();

        let rust_only = tmp.path().join("logic");
        std::fs::create_dir_all(rust_only.join("src")).unwrap();
        std::fs::write(rust_only.join("src/mod.rs"), "").unwrap();
        validate_shape(&rust_only).unwrap();

        let headless = tmp.path().join("headless");
        std::fs::create_dir_all(headless.join("src")).unwrap();
        std::fs::write(headless.join("src/other.rs"), "").unwrap();
        let error = validate_shape(&headless).unwrap_err();
        assert!(error.to_string().contains("mod.rs"), "{error}");

        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let error = validate_shape(&empty).unwrap_err();
        assert!(error.to_string().contains("neither src/"), "{error}");
    }

    // ---- the install ----

    #[test]
    fn a_confirmed_install_copies_the_tree_stamps_it_and_stops_short_of_publishing() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");
        let stack = FakeStack::new();

        add(
            &project,
            &stack.runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            false,
        )
        .unwrap();

        let installed = project.packages_dir().join("greeter");
        assert!(installed.join("src/mod.rs").is_file());
        let recorded = ProvenanceStamp::read(&installed).expect("no provenance stamp");
        assert_eq!(recorded.source_kind, stamp::SOURCE_LOCAL);
        assert_eq!(recorded.source, source.to_string_lossy());
        assert_eq!(
            recorded.content_identity,
            stamp::content_identity(&installed).unwrap(),
            "the stamp must record the identity of what was actually copied"
        );
        // The remaining steps are PRINTED, never run: nothing here may touch the node.
        for call in stack.rendered() {
            assert!(!call.contains("spacetime publish"), "{call}");
            assert!(!call.contains("--pack-client"), "{call}");
        }
    }

    #[test]
    fn the_copy_is_a_copy_never_a_link() {
        // A linked Package would compile from a folder outside the checkout, so preflight, publish
        // and client sync would each read whatever it said at the time.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");
        add(
            &project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap();

        let installed = project.packages_dir().join("greeter");
        assert!(!installed
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::write(source.join("src/mod.rs"), "// changed at the source\n").unwrap();
        assert!(
            std::fs::read_to_string(installed.join("src/mod.rs"))
                .unwrap()
                .contains("game_hook"),
            "editing the Package Source must not change the installed Package"
        );
    }

    #[test]
    fn an_answer_other_than_yes_copies_nothing() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");

        let error = add(
            &project,
            &FakeStack::new().runner(),
            &Answer("no"),
            source.to_str().unwrap(),
            false,
        )
        .unwrap_err();

        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE);
        assert!(!project.packages_dir().join("greeter").exists(), "{error}");
    }

    #[test]
    fn an_invalid_name_or_shape_is_refused_before_anything_is_copied() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);

        let bad_name = tmp.path().join("sources").join("2fast");
        std::fs::create_dir_all(bad_name.join("src")).unwrap();
        std::fs::write(bad_name.join("src/mod.rs"), "").unwrap();
        let error = add(
            &project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            bad_name.to_str().unwrap(),
            true,
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), crate::error::EXIT_USAGE, "{error}");
        assert!(
            !project.packages_dir().exists(),
            "nothing may be created: {error}"
        );
    }

    #[test]
    fn a_name_either_inventory_already_holds_is_refused_before_the_copy() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");

        // Enabled collision.
        std::fs::create_dir_all(project.packages_dir().join("greeter")).unwrap();
        let error = check_collision(&project, &PackageName::parse("greeter").unwrap()).unwrap_err();
        assert!(error.to_string().contains("enabled"), "{error}");

        // ...and a DISABLED one, which the build cannot see today but would collide the moment it
        // was re-enabled.
        std::fs::remove_dir_all(project.packages_dir().join("greeter")).unwrap();
        std::fs::create_dir_all(project.packages_disabled_dir().join("greeter")).unwrap();
        let error = add(
            &project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("disabled"), "{error}");
        assert!(!project.packages_dir().join("greeter").exists(), "{error}");
    }

    #[test]
    fn a_name_that_folds_onto_an_installed_module_is_refused_too() {
        // `my-package` and `my_package` both generate `pkg_my_package`; installing both would give
        // the module two modules with one name.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::create_dir_all(project.packages_dir().join("my-package")).unwrap();

        let error =
            check_collision(&project, &PackageName::parse("my_package").unwrap()).unwrap_err();

        assert!(error.to_string().contains("pkg_my_package"), "{error}");
    }

    #[test]
    fn a_failed_preflight_publishes_nothing_and_says_how_to_undo_the_copy() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");
        let stack = FakeStack::new().fail_on("cargo check", "the Package does not compile");

        let error = add(
            &project,
            &stack.runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap_err();

        let installed = project.packages_dir().join("greeter");
        assert!(error.to_string().contains("preflight failed"), "{error}");
        // The exact undo, not "remove it manually" — the operator is holding a half-finished
        // install and the path is the one thing they need.
        assert!(
            error
                .to_string()
                .contains(&format!("rm -rf {}", installed.display())),
            "{error}"
        );
        assert!(
            installed.is_dir(),
            "the copy stays so it can be fixed in place"
        );
        for call in stack.rendered() {
            assert!(!call.contains("spacetime publish"), "{call}");
        }
    }

    #[test]
    fn a_symlink_inside_the_candidate_is_refused() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");
        let outside = tmp.path().join("elsewhere.rs");
        std::fs::write(&outside, "// not part of the reviewed folder\n").unwrap();
        std::os::unix::fs::symlink(&outside, source.join("src/linked.rs")).unwrap();

        let error = add(
            &project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains("symlink"), "{error}");
    }

    // ---- `packages list` ----

    #[test]
    fn list_reports_both_inventories_and_the_drift_of_each() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        let source = candidate(&tmp, "greeter");
        add(
            &project,
            &FakeStack::new().runner(),
            &Answer("yes"),
            source.to_str().unwrap(),
            true,
        )
        .unwrap();
        let installed = project.packages_dir().join("greeter");

        let installed_packages = inventory(&project).unwrap();
        assert_eq!(installed_packages.len(), 1);
        assert_eq!(installed_packages[0].state, PackageState::Enabled);
        let recorded = installed_packages[0].stamp.clone().unwrap();
        assert_eq!(
            drift(&recorded, &stamp::content_identity(&installed).unwrap()),
            "clean"
        );

        // Edit the installed copy: the recorded identity no longer describes what is on disk.
        std::fs::write(installed.join("src/mod.rs"), "// edited in place\n").unwrap();
        assert!(
            drift(&recorded, &stamp::content_identity(&installed).unwrap()).contains("DRIFTED")
        );

        // A disabled Package is in the inventory too, and `list` renders both.
        std::fs::create_dir_all(project.packages_disabled_dir().join("retired/src")).unwrap();
        let installed_packages = inventory(&project).unwrap();
        assert_eq!(installed_packages.len(), 2);
        assert_eq!(installed_packages[1].state, PackageState::Disabled);
        list(&project).unwrap();
    }

    #[test]
    fn a_package_with_no_stamp_is_described_rather_than_crashing_the_listing() {
        // The pre-existing case: a folder somebody dropped into packages/ by hand, or one that
        // predates `packages add`.
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        std::fs::create_dir_all(project.packages_dir().join("handmade/src")).unwrap();
        std::fs::write(
            project.packages_dir().join("handmade/src/mod.rs"),
            "pub fn helper() {}\n",
        )
        .unwrap();

        let installed_packages = inventory(&project).unwrap();

        assert_eq!(installed_packages.len(), 1);
        assert!(installed_packages[0].stamp.is_none());
        list(&project).unwrap();
    }

    #[test]
    fn an_empty_checkout_lists_nothing_and_says_how_to_install() {
        let tmp = TempDir::new().unwrap();
        let project = checkout(&tmp);
        assert!(inventory(&project).unwrap().is_empty());
        list(&project).unwrap();
    }
}
