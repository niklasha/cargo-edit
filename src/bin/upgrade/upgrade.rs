use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use cargo_edit::{
    colorize_stderr, find, get_latest_dependency, manifest_from_pkgid, registry_url,
    set_dep_version, shell_status, shell_warn, update_registry_index, CargoResult, Context,
    CrateSpec, Dependency, LocalManifest,
};
use clap::Args;
use semver::{Op, VersionReq};
use termcolor::{Color, ColorSpec, StandardStream, WriteColor};

/// Upgrade dependencies as specified in the local manifest file (i.e. Cargo.toml).
#[derive(Debug, Args)]
#[clap(version)]
#[clap(after_help = "\
This command differs from `cargo update`, which updates the dependency versions recorded in the \
local lock file (Cargo.lock).

If `<dependency>`(s) are provided, only the specified dependencies will be upgraded. The version \
to upgrade to for each can be specified with e.g. `docopt@0.8.0` or `serde@>=0.9,<2.0`.

Dev, build, and all target dependencies will also be upgraded. Only dependencies from crates.io \
are supported. Git/path dependencies will be ignored.

All packages in the workspace will be upgraded if the `--workspace` flag is supplied. The \
`--workspace` flag may be supplied in the presence of a virtual manifest.

If the '--to-lockfile' flag is supplied, all dependencies will be upgraded to the currently locked \
version as recorded in the Cargo.lock file. This flag requires that the Cargo.lock file is \
up-to-date. If the lock file is missing, or it needs to be updated, cargo-upgrade will exit with \
an error. If the '--to-lockfile' flag is supplied then the network won't be accessed.")]
pub struct UpgradeArgs {
    /// Crates to be upgraded.
    dependency: Vec<String>,

    /// Path to the manifest to upgrade
    #[clap(
        long,
        value_name = "PATH",
        parse(from_os_str),
        conflicts_with = "pkgid"
    )]
    manifest_path: Option<PathBuf>,

    /// Package id of the crate to add this dependency to.
    #[clap(
        long = "package",
        short = 'p',
        value_name = "PKGID",
        conflicts_with = "manifest-path",
        conflicts_with = "all",
        conflicts_with = "workspace"
    )]
    pkgid: Option<String>,

    /// Upgrade all packages in the workspace.
    #[clap(
        long,
        help = "[deprecated in favor of `--workspace`]",
        conflicts_with = "workspace",
        conflicts_with = "pkgid"
    )]
    all: bool,

    /// Upgrade all packages in the workspace.
    #[clap(long, conflicts_with = "all", conflicts_with = "pkgid")]
    workspace: bool,

    /// Include prerelease versions when fetching from crates.io (e.g. 0.6.0-alpha').
    #[clap(long)]
    allow_prerelease: bool,

    /// Print changes to be made without making them.
    #[clap(long)]
    dry_run: bool,

    /// Only update a dependency if the new version is semver incompatible.
    #[clap(long, conflicts_with = "to-lockfile")]
    skip_compatible: bool,

    /// Only update a dependency if it is not currently pinned in the manifest.
    /// "Pinned" refers to dependencies with a '=' or '<' or '<=' version requirement
    #[clap(long)]
    skip_pinned: bool,

    /// Run without accessing the network
    #[clap(long)]
    offline: bool,

    /// Upgrade all packages to the version in the lockfile.
    #[clap(long, conflicts_with = "dependency")]
    to_lockfile: bool,

    /// Crates to exclude and not upgrade.
    #[clap(long)]
    exclude: Vec<String>,

    /// Unstable (nightly-only) flags
    #[clap(short = 'Z', value_name = "FLAG", global = true, arg_enum)]
    unstable_features: Vec<UnstableOptions>,
}

impl UpgradeArgs {
    pub fn exec(self) -> CargoResult<()> {
        exec(self)
    }

    fn workspace(&self) -> bool {
        self.all || self.workspace
    }

    fn resolve_targets(&self) -> CargoResult<Vec<(LocalManifest, cargo_metadata::Package)>> {
        if self.workspace() {
            resolve_all(self.manifest_path.as_deref())
        } else if let Some(pkgid) = self.pkgid.as_deref() {
            resolve_pkgid(self.manifest_path.as_deref(), pkgid)
        } else {
            resolve_local_one(self.manifest_path.as_deref())
        }
    }

    fn preserve_precision(&self) -> bool {
        self.unstable_features
            .contains(&UnstableOptions::PreservePrecision)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ArgEnum)]
enum UnstableOptions {
    PreservePrecision,
}

/// Main processing function. Allows us to return a `Result` so that `main` can print pretty error
/// messages.
fn exec(args: UpgradeArgs) -> CargoResult<()> {
    if args.all {
        deprecated_message("The flag `--all` has been deprecated in favor of `--workspace`")?;
    }

    if !args.offline && !args.to_lockfile && std::env::var("CARGO_IS_TEST").is_err() {
        let url = registry_url(&find(args.manifest_path.as_deref())?, None)?;
        update_registry_index(&url, false)?;
    }

    let manifests = args.resolve_targets()?;
    let locked = if std::env::var("CARGO_IS_TEST").is_err() {
        load_lockfile(&manifests)?
    } else {
        load_lockfile(&manifests).unwrap_or_default()
    };
    let preserve_precision = args.preserve_precision();

    let selected_dependencies = args
        .dependency
        .iter()
        .map(|name| {
            let spec = CrateSpec::resolve(name)?;
            Ok((spec.name, spec.version_req))
        })
        .collect::<CargoResult<BTreeMap<_, _>>>()?;

    let mut updated_registries = BTreeSet::new();
    for (mut manifest, package) in manifests {
        let manifest_path = manifest.path.clone();
        shell_status("Checking", &format!("{}'s dependencies", package.name))?;
        for dep_table in manifest.get_dependency_tables_mut() {
            for (dep_key, dep_item) in dep_table.iter_mut() {
                if !selected_dependencies.is_empty()
                    && !selected_dependencies.contains_key(dep_key.get())
                {
                    continue;
                }
                if args.exclude.contains(&dep_key.get().to_owned()) {
                    continue;
                }
                let dependency =
                    match Dependency::from_toml(&manifest_path, dep_key.get(), dep_item) {
                        Ok(dependency) => dependency,
                        Err(_) => {
                            continue;
                        }
                    };
                let old_version = match dependency.source.as_ref().and_then(|s| s.as_registry()) {
                    Some(registry) => registry.version.clone(),
                    None => {
                        continue;
                    }
                };
                if args.skip_pinned {
                    if dependency.rename.is_some() {
                        continue;
                    }

                    if let Ok(version_req) = VersionReq::parse(&old_version) {
                        if version_req.comparators.iter().any(|comparator| {
                            matches!(comparator.op, Op::Exact | Op::Less | Op::LessEq)
                        }) {
                            continue;
                        }
                    }
                }

                let mut new_version = if let Some(Some(new_version)) =
                    selected_dependencies.get(dependency.toml_key())
                {
                    new_version.to_owned()
                } else {
                    // Not checking `selected_dependencies.is_empty`, it was checked earlier
                    let new_version = if args.to_lockfile {
                        find_locked_version(&dependency.name, &old_version, &locked).ok_or_else(
                            || anyhow::format_err!("{} is not in block file", dependency.name),
                        )
                    } else {
                        // Update indices for any alternative registries, unless
                        // we're offline.
                        let registry_url = dependency
                            .registry()
                            .map(|registry| registry_url(&manifest_path, Some(registry)))
                            .transpose()?;
                        if !args.offline && std::env::var("CARGO_IS_TEST").is_err() {
                            if let Some(registry_url) = &registry_url {
                                if updated_registries.insert(registry_url.to_owned()) {
                                    update_registry_index(registry_url, false)?;
                                }
                            }
                        }
                        let is_prerelease = old_version.contains('-');
                        let allow_prerelease = args.allow_prerelease || is_prerelease;
                        get_latest_dependency(
                            &dependency.name,
                            allow_prerelease,
                            &manifest_path,
                            registry_url.as_ref(),
                        )
                        .map(|d| {
                            d.version()
                                .expect("registry packages always have a version")
                                .to_owned()
                        })
                    };
                    let new_version = match new_version {
                        Ok(new_version) => new_version,
                        Err(_) => {
                            continue;
                        }
                    };
                    new_version
                };
                if preserve_precision {
                    let new_ver: semver::Version = new_version.parse()?;
                    match cargo_edit::upgrade_requirement(&old_version, &new_ver) {
                        Ok(Some(version)) => {
                            new_version = version;
                        }
                        Err(_) => {}
                        _ => {
                            new_version = old_version.clone();
                        }
                    }
                }
                if args.skip_compatible && old_version_compatible(&old_version, &new_version) {
                    continue;
                }
                if new_version == old_version {
                    continue;
                }
                print_upgrade(dependency.toml_key(), &old_version, &new_version)?;
                set_dep_version(dep_item, &new_version)?;
            }
        }
        if !args.dry_run {
            manifest.write()?;
        }
    }

    if args.dry_run {
        shell_warn("aborting upgrade due to dry run")?;
    }

    Ok(())
}

fn load_lockfile(
    targets: &[(LocalManifest, cargo_metadata::Package)],
) -> CargoResult<Vec<cargo_metadata::Package>> {
    // Get locked dependencies. For workspaces with multiple Cargo.toml
    // files, there is only a single lockfile, so it suffices to get
    // metadata for any one of Cargo.toml files.
    let (manifest, _package) = targets
        .get(0)
        .ok_or_else(|| anyhow::format_err!("Invalid cargo config"))?;
    let mut cmd = cargo_metadata::MetadataCommand::new();
    cmd.manifest_path(manifest.path.clone());
    cmd.features(cargo_metadata::CargoOpt::AllFeatures);
    cmd.other_options(vec!["--locked".to_string()]);

    let result = cmd.exec().with_context(|| "Invalid manifest")?;

    let locked = result
        .packages
        .into_iter()
        .filter(|p| p.source.is_some()) // Source is none for local packages
        .collect::<Vec<_>>();

    Ok(locked)
}

fn find_locked_version(
    dep_name: &str,
    old_version: &str,
    locked: &[cargo_metadata::Package],
) -> Option<String> {
    let req = semver::VersionReq::parse(&old_version).ok()?;
    for p in locked {
        if dep_name == p.name && req.matches(&p.version) {
            return Some(p.version.to_string());
        }
    }
    None
}

/// Get all manifests in the workspace.
fn resolve_all(
    manifest_path: Option<&Path>,
) -> CargoResult<Vec<(LocalManifest, cargo_metadata::Package)>> {
    let mut cmd = cargo_metadata::MetadataCommand::new();
    cmd.no_deps();
    if let Some(path) = manifest_path {
        cmd.manifest_path(path);
    }
    let result = cmd
        .exec()
        .with_context(|| "Failed to get workspace metadata")?;
    result
        .packages
        .into_iter()
        .map(|package| {
            Ok((
                LocalManifest::try_new(Path::new(&package.manifest_path))?,
                package,
            ))
        })
        .collect::<CargoResult<Vec<_>>>()
}

fn resolve_pkgid(
    manifest_path: Option<&Path>,
    pkgid: &str,
) -> CargoResult<Vec<(LocalManifest, cargo_metadata::Package)>> {
    let package = manifest_from_pkgid(manifest_path, pkgid)?;
    let manifest = LocalManifest::try_new(Path::new(&package.manifest_path))?;
    Ok(vec![(manifest, package)])
}

/// Get the manifest specified by the manifest path. Try to make an educated guess if no path is
/// provided.
fn resolve_local_one(
    manifest_path: Option<&Path>,
) -> CargoResult<Vec<(LocalManifest, cargo_metadata::Package)>> {
    let resolved_manifest_path: String = find(manifest_path)?.to_string_lossy().into();

    let manifest = LocalManifest::find(manifest_path)?;

    let mut cmd = cargo_metadata::MetadataCommand::new();
    cmd.no_deps();
    if let Some(path) = manifest_path {
        cmd.manifest_path(path);
    }
    let result = cmd.exec().with_context(|| "Invalid manifest")?;
    let packages = result.packages;
    let package = packages
        .iter()
        .find(|p| p.manifest_path == resolved_manifest_path)
        // If we have successfully got metadata, but our manifest path does not correspond to a
        // package, we must have been called against a virtual manifest.
        .with_context(|| {
            "Found virtual manifest, but this command requires running against an \
                 actual package in this workspace. Try adding `--workspace`."
        })?;

    Ok(vec![(manifest, package.to_owned())])
}

fn old_version_compatible(old_version: &str, mut new_version: &str) -> bool {
    let old_version = match VersionReq::parse(old_version) {
        Ok(req) => req,
        Err(_) => return false,
    };

    let new_req = VersionReq::parse(new_version);
    assert!(new_req.is_ok(), "{}", new_req.unwrap_err());
    let first_char = new_version.chars().next();
    if !first_char.unwrap_or('0').is_ascii_digit() {
        new_version = new_version.strip_prefix(first_char.unwrap()).unwrap();
    }
    let new_version = match semver::Version::parse(new_version) {
        Ok(new_version) => new_version,
        // HACK: Skip compatibility checks on incomplete version reqs
        Err(_) => return false,
    };

    old_version.matches(&new_version)
}

fn deprecated_message(message: &str) -> CargoResult<()> {
    let colorchoice = colorize_stderr();
    let mut output = StandardStream::stderr(colorchoice);
    output
        .set_color(ColorSpec::new().set_fg(Some(Color::Red)).set_bold(true))
        .with_context(|| "Failed to set output colour")?;
    writeln!(output, "{}", message).with_context(|| "Failed to write deprecated message")?;
    output
        .set_color(&ColorSpec::new())
        .with_context(|| "Failed to clear output colour")?;
    Ok(())
}

/// Print a message if the new dependency version is different from the old one.
fn print_upgrade(dep_name: &str, old_version: &str, new_version: &str) -> CargoResult<()> {
    let old_version = format_version_req(old_version);
    let new_version = format_version_req(new_version);

    shell_status(
        "Upgrading",
        &format!("{dep_name}: {old_version} -> {new_version}"),
    )?;

    Ok(())
}

fn format_version_req(version: &str) -> String {
    if version.chars().next().unwrap_or('0').is_ascii_digit() {
        format!("v{}", version)
    } else {
        version.to_owned()
    }
}
