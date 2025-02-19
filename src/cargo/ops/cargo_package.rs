use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::prelude::*;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crate::core::compiler::{BuildConfig, CompileMode, DefaultExecutor, Executor};
use crate::core::resolver::CliFeatures;
use crate::core::{Feature, Shell, Verbosity, Workspace};
use crate::core::{Package, PackageId, PackageSet, Resolve, Source, SourceId};
use crate::sources::PathSource;
use crate::util::errors::CargoResult;
use crate::util::toml::TomlManifest;
use crate::util::{self, restricted_names, Config, FileLock};
use crate::{drop_println, ops};
use anyhow::Context as _;
use cargo_util::paths;
use flate2::read::GzDecoder;
use flate2::{Compression, GzBuilder};
use log::debug;
use serde::Serialize;
use tar::{Archive, Builder, EntryType, Header, HeaderMode};

pub struct PackageOpts<'cfg> {
    pub config: &'cfg Config,
    pub list: bool,
    pub check_metadata: bool,
    pub allow_dirty: bool,
    pub verify: bool,
    pub jobs: Option<u32>,
    pub to_package: ops::Packages,
    pub targets: Vec<String>,
    pub cli_features: CliFeatures,
}

const VCS_INFO_FILE: &str = ".cargo_vcs_info.json";

struct ArchiveFile {
    /// The relative path in the archive (not including the top-level package
    /// name directory).
    rel_path: PathBuf,
    /// String variant of `rel_path`, for convenience.
    rel_str: String,
    /// The contents to add to the archive.
    contents: FileContents,
}

enum FileContents {
    /// Absolute path to the file on disk to add to the archive.
    OnDisk(PathBuf),
    /// Generates a file.
    Generated(GeneratedFile),
}

enum GeneratedFile {
    /// Generates `Cargo.toml` by rewriting the original.
    Manifest,
    /// Generates `Cargo.lock` in some cases (like if there is a binary).
    Lockfile,
    /// Adds a `.cargo_vcs_info.json` file if in a (clean) git repo.
    VcsInfo(VcsInfo),
}

#[derive(Serialize)]
struct VcsInfo {
    git: GitVcsInfo,
}

#[derive(Serialize)]
struct GitVcsInfo {
    sha1: String,
}

pub fn package_one(
    ws: &Workspace<'_>,
    pkg: &Package,
    opts: &PackageOpts<'_>,
) -> CargoResult<Option<FileLock>> {
    let config = ws.config();
    let mut src = PathSource::new(pkg.root(), pkg.package_id().source_id(), config);
    src.update()?;

    if opts.check_metadata {
        check_metadata(pkg, config)?;
    }

    if !pkg.manifest().exclude().is_empty() && !pkg.manifest().include().is_empty() {
        config.shell().warn(
            "both package.include and package.exclude are specified; \
             the exclude list will be ignored",
        )?;
    }
    let src_files = src.list_files(pkg)?;

    // Check (git) repository state, getting the current commit hash if not
    // dirty.
    let vcs_info = if !opts.allow_dirty {
        // This will error if a dirty repo is found.
        check_repo_state(pkg, &src_files, config)?
    } else {
        None
    };

    let ar_files = build_ar_list(ws, pkg, src_files, vcs_info)?;

    if opts.list {
        for ar_file in ar_files {
            drop_println!(config, "{}", ar_file.rel_str);
        }

        return Ok(None);
    }

    // Check that the package dependencies are safe to deploy.
    for dep in pkg.dependencies() {
        super::check_dep_has_version(dep, false)?;
    }

    let filename = format!("{}-{}.crate", pkg.name(), pkg.version());
    let dir = ws.target_dir().join("package");
    let mut dst = {
        let tmp = format!(".{}", filename);
        dir.open_rw(&tmp, config, "package scratch space")?
    };

    // Package up and test a temporary tarball and only move it to the final
    // location if it actually passes all our tests. Any previously existing
    // tarball can be assumed as corrupt or invalid, so we just blow it away if
    // it exists.
    config
        .shell()
        .status("Packaging", pkg.package_id().to_string())?;
    dst.file().set_len(0)?;
    tar(ws, pkg, ar_files, dst.file(), &filename)
        .with_context(|| "failed to prepare local package for uploading")?;
    if opts.verify {
        dst.seek(SeekFrom::Start(0))?;
        run_verify(ws, pkg, &dst, opts).with_context(|| "failed to verify package tarball")?
    }

    dst.seek(SeekFrom::Start(0))?;
    let src_path = dst.path();
    let dst_path = dst.parent().join(&filename);
    fs::rename(&src_path, &dst_path)
        .with_context(|| "failed to move temporary tarball into final location")?;

    return Ok(Some(dst));
}

pub fn package(ws: &Workspace<'_>, opts: &PackageOpts<'_>) -> CargoResult<Option<Vec<FileLock>>> {
    let pkgs = ws.members_with_features(
        &opts.to_package.to_package_id_specs(ws)?,
        &opts.cli_features,
    )?;

    let mut dsts = Vec::with_capacity(pkgs.len());

    if ws.root().join("Cargo.lock").exists() {
        // Make sure the Cargo.lock is up-to-date and valid.
        let _ = ops::resolve_ws(ws)?;
        // If Cargo.lock does not exist, it will be generated by `build_lock`
        // below, and will be validated during the verification step.
    }

    for (pkg, cli_features) in pkgs {
        let result = package_one(
            ws,
            pkg,
            &PackageOpts {
                config: opts.config,
                list: opts.list,
                check_metadata: opts.check_metadata,
                allow_dirty: opts.allow_dirty,
                verify: opts.verify,
                jobs: opts.jobs,
                to_package: ops::Packages::Default,
                targets: opts.targets.clone(),
                cli_features: cli_features,
            },
        )?;

        if !opts.list {
            dsts.push(result.unwrap());
        }
    }

    if opts.list {
        // We're just listing, so there's no file output
        Ok(None)
    } else {
        Ok(Some(dsts))
    }
}

/// Builds list of files to archive.
fn build_ar_list(
    ws: &Workspace<'_>,
    pkg: &Package,
    src_files: Vec<PathBuf>,
    vcs_info: Option<VcsInfo>,
) -> CargoResult<Vec<ArchiveFile>> {
    let mut result = Vec::new();
    let root = pkg.root();
    for src_file in src_files {
        let rel_path = src_file.strip_prefix(&root)?.to_path_buf();
        check_filename(&rel_path, &mut ws.config().shell())?;
        let rel_str = rel_path
            .to_str()
            .ok_or_else(|| {
                anyhow::format_err!("non-utf8 path in source directory: {}", rel_path.display())
            })?
            .to_string();
        match rel_str.as_ref() {
            "Cargo.toml" => {
                result.push(ArchiveFile {
                    rel_path: PathBuf::from("Cargo.toml.orig"),
                    rel_str: "Cargo.toml.orig".to_string(),
                    contents: FileContents::OnDisk(src_file),
                });
                result.push(ArchiveFile {
                    rel_path,
                    rel_str,
                    contents: FileContents::Generated(GeneratedFile::Manifest),
                });
            }
            "Cargo.lock" => continue,
            VCS_INFO_FILE => anyhow::bail!(
                "invalid inclusion of reserved file name \
                     {} in package source",
                VCS_INFO_FILE
            ),
            _ => {
                result.push(ArchiveFile {
                    rel_path,
                    rel_str,
                    contents: FileContents::OnDisk(src_file),
                });
            }
        }
    }
    if pkg.include_lockfile() {
        result.push(ArchiveFile {
            rel_path: PathBuf::from("Cargo.lock"),
            rel_str: "Cargo.lock".to_string(),
            contents: FileContents::Generated(GeneratedFile::Lockfile),
        });
    }
    if let Some(vcs_info) = vcs_info {
        result.push(ArchiveFile {
            rel_path: PathBuf::from(VCS_INFO_FILE),
            rel_str: VCS_INFO_FILE.to_string(),
            contents: FileContents::Generated(GeneratedFile::VcsInfo(vcs_info)),
        });
    }
    if let Some(license_file) = &pkg.manifest().metadata().license_file {
        let license_path = Path::new(license_file);
        let abs_license_path = paths::normalize_path(&pkg.root().join(license_path));
        if abs_license_path.exists() {
            match abs_license_path.strip_prefix(&pkg.root()) {
                Ok(rel_license_path) => {
                    if !result.iter().any(|ar| ar.rel_path == rel_license_path) {
                        result.push(ArchiveFile {
                            rel_path: rel_license_path.to_path_buf(),
                            rel_str: rel_license_path
                                .to_str()
                                .expect("everything was utf8")
                                .to_string(),
                            contents: FileContents::OnDisk(abs_license_path),
                        });
                    }
                }
                Err(_) => {
                    // The license exists somewhere outside of the package.
                    let license_name = license_path.file_name().unwrap();
                    if result
                        .iter()
                        .any(|ar| ar.rel_path.file_name().unwrap() == license_name)
                    {
                        ws.config().shell().warn(&format!(
                            "license-file `{}` appears to be a path outside of the package, \
                            but there is already a file named `{}` in the root of the package. \
                            The archived crate will contain the copy in the root of the package. \
                            Update the license-file to point to the path relative \
                            to the root of the package to remove this warning.",
                            license_file,
                            license_name.to_str().unwrap()
                        ))?;
                    } else {
                        result.push(ArchiveFile {
                            rel_path: PathBuf::from(license_name),
                            rel_str: license_name.to_str().unwrap().to_string(),
                            contents: FileContents::OnDisk(abs_license_path),
                        });
                    }
                }
            }
        } else {
            let rel_msg = if license_path.is_absolute() {
                "".to_string()
            } else {
                format!(" (relative to `{}`)", pkg.root().display())
            };
            ws.config().shell().warn(&format!(
                "license-file `{}` does not appear to exist{}.\n\
                Please update the license-file setting in the manifest at `{}`\n\
                This may become a hard error in the future.",
                license_path.display(),
                rel_msg,
                pkg.manifest_path().display()
            ))?;
        }
    }
    result.sort_unstable_by(|a, b| a.rel_path.cmp(&b.rel_path));

    Ok(result)
}

/// Construct `Cargo.lock` for the package to be published.
fn build_lock(ws: &Workspace<'_>, orig_pkg: &Package) -> CargoResult<String> {
    let config = ws.config();
    let orig_resolve = ops::load_pkg_lockfile(ws)?;

    // Convert Package -> TomlManifest -> Manifest -> Package
    let toml_manifest = Rc::new(
        orig_pkg
            .manifest()
            .original()
            .prepare_for_publish(ws, orig_pkg.root())?,
    );
    let package_root = orig_pkg.root();
    let source_id = orig_pkg.package_id().source_id();
    let (manifest, _nested_paths) =
        TomlManifest::to_real_manifest(&toml_manifest, source_id, package_root, config)?;
    let new_pkg = Package::new(manifest, orig_pkg.manifest_path());

    // Regenerate Cargo.lock using the old one as a guide.
    let tmp_ws = Workspace::ephemeral(new_pkg, ws.config(), None, true)?;
    let (pkg_set, mut new_resolve) = ops::resolve_ws(&tmp_ws)?;

    if let Some(orig_resolve) = orig_resolve {
        compare_resolve(config, tmp_ws.current()?, &orig_resolve, &new_resolve)?;
    }
    check_yanked(config, &pkg_set, &new_resolve)?;

    ops::resolve_to_string(&tmp_ws, &mut new_resolve)
}

// Checks that the package has some piece of metadata that a human can
// use to tell what the package is about.
fn check_metadata(pkg: &Package, config: &Config) -> CargoResult<()> {
    let md = pkg.manifest().metadata();

    let mut missing = vec![];

    macro_rules! lacking {
        ($( $($field: ident)||* ),*) => {{
            $(
                if $(md.$field.as_ref().map_or(true, |s| s.is_empty()))&&* {
                    $(missing.push(stringify!($field).replace("_", "-"));)*
                }
            )*
        }}
    }
    lacking!(
        description,
        license || license_file,
        documentation || homepage || repository
    );

    if !missing.is_empty() {
        let mut things = missing[..missing.len() - 1].join(", ");
        // `things` will be empty if and only if its length is 1 (i.e., the only case
        // to have no `or`).
        if !things.is_empty() {
            things.push_str(" or ");
        }
        things.push_str(missing.last().unwrap());

        config.shell().warn(&format!(
            "manifest has no {things}.\n\
             See https://doc.rust-lang.org/cargo/reference/manifest.html#package-metadata for more info.",
            things = things
        ))?
    }

    Ok(())
}

/// Checks if the package source is in a *git* DVCS repository. If *git*, and
/// the source is *dirty* (e.g., has uncommitted changes) then `bail!` with an
/// informative message. Otherwise return the sha1 hash of the current *HEAD*
/// commit, or `None` if no repo is found.
fn check_repo_state(
    p: &Package,
    src_files: &[PathBuf],
    config: &Config,
) -> CargoResult<Option<VcsInfo>> {
    if let Ok(repo) = git2::Repository::discover(p.root()) {
        if let Some(workdir) = repo.workdir() {
            debug!("found a git repo at {:?}", workdir);
            let path = p.manifest_path();
            let path = path.strip_prefix(workdir).unwrap_or(path);
            if let Ok(status) = repo.status_file(path) {
                if (status & git2::Status::IGNORED).is_empty() {
                    debug!(
                        "found (git) Cargo.toml at {:?} in workdir {:?}",
                        path, workdir
                    );
                    return Ok(Some(VcsInfo {
                        git: git(p, src_files, &repo)?,
                    }));
                }
            }
            config.shell().verbose(|shell| {
                shell.warn(format!(
                    "No (git) Cargo.toml found at `{}` in workdir `{}`",
                    path.display(),
                    workdir.display()
                ))
            })?;
        }
    } else {
        config.shell().verbose(|shell| {
            shell.warn(format!("No (git) VCS found for `{}`", p.root().display()))
        })?;
    }

    // No VCS with a checked in `Cargo.toml` found, so we don't know if the
    // directory is dirty or not, thus we have to assume that it's clean.
    return Ok(None);

    fn git(p: &Package, src_files: &[PathBuf], repo: &git2::Repository) -> CargoResult<GitVcsInfo> {
        // This is a collection of any dirty or untracked files. This covers:
        // - new/modified/deleted/renamed/type change (index or worktree)
        // - untracked files (which are "new" worktree files)
        // - ignored (in case the user has an `include` directive that
        //   conflicts with .gitignore).
        let mut dirty_files = Vec::new();
        collect_statuses(repo, &mut dirty_files)?;
        // Include each submodule so that the error message can provide
        // specifically *which* files in a submodule are modified.
        status_submodules(repo, &mut dirty_files)?;

        // Find the intersection of dirty in git, and the src_files that would
        // be packaged. This is a lazy n^2 check, but seems fine with
        // thousands of files.
        let dirty_src_files: Vec<String> = src_files
            .iter()
            .filter(|src_file| dirty_files.iter().any(|path| src_file.starts_with(path)))
            .map(|path| {
                path.strip_prefix(p.root())
                    .unwrap_or(path)
                    .display()
                    .to_string()
            })
            .collect();
        if dirty_src_files.is_empty() {
            let rev_obj = repo.revparse_single("HEAD")?;
            Ok(GitVcsInfo {
                sha1: rev_obj.id().to_string(),
            })
        } else {
            anyhow::bail!(
                "{} files in the working directory contain changes that were \
                 not yet committed into git:\n\n{}\n\n\
                 to proceed despite this and include the uncommitted changes, pass the `--allow-dirty` flag",
                dirty_src_files.len(),
                dirty_src_files.join("\n")
            )
        }
    }

    // Helper to collect dirty statuses for a single repo.
    fn collect_statuses(
        repo: &git2::Repository,
        dirty_files: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        let mut status_opts = git2::StatusOptions::new();
        // Exclude submodules, as they are being handled manually by recursing
        // into each one so that details about specific files can be
        // retrieved.
        status_opts
            .exclude_submodules(true)
            .include_ignored(true)
            .include_untracked(true);
        let repo_statuses = repo.statuses(Some(&mut status_opts)).with_context(|| {
            format!(
                "failed to retrieve git status from repo {}",
                repo.path().display()
            )
        })?;
        let workdir = repo.workdir().unwrap();
        let this_dirty = repo_statuses.iter().filter_map(|entry| {
            let path = entry.path().expect("valid utf-8 path");
            if path.ends_with("Cargo.lock") && entry.status() == git2::Status::IGNORED {
                // It is OK to include Cargo.lock even if it is ignored.
                return None;
            }
            // Use an absolute path, so that comparing paths is easier
            // (particularly with submodules).
            Some(workdir.join(path))
        });
        dirty_files.extend(this_dirty);
        Ok(())
    }

    // Helper to collect dirty statuses while recursing into submodules.
    fn status_submodules(
        repo: &git2::Repository,
        dirty_files: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        for submodule in repo.submodules()? {
            // Ignore submodules that don't open, they are probably not initialized.
            // If its files are required, then the verification step should fail.
            if let Ok(sub_repo) = submodule.open() {
                status_submodules(&sub_repo, dirty_files)?;
                collect_statuses(&sub_repo, dirty_files)?;
            }
        }
        Ok(())
    }
}

fn tar(
    ws: &Workspace<'_>,
    pkg: &Package,
    ar_files: Vec<ArchiveFile>,
    dst: &File,
    filename: &str,
) -> CargoResult<()> {
    // Prepare the encoder and its header.
    let filename = Path::new(filename);
    let encoder = GzBuilder::new()
        .filename(paths::path2bytes(filename)?)
        .write(dst, Compression::best());

    // Put all package files into a compressed archive.
    let mut ar = Builder::new(encoder);
    let config = ws.config();

    let base_name = format!("{}-{}", pkg.name(), pkg.version());
    let base_path = Path::new(&base_name);
    for ar_file in ar_files {
        let ArchiveFile {
            rel_path,
            rel_str,
            contents,
        } = ar_file;
        let ar_path = base_path.join(&rel_path);
        config
            .shell()
            .verbose(|shell| shell.status("Archiving", &rel_str))?;
        let mut header = Header::new_gnu();
        match contents {
            FileContents::OnDisk(disk_path) => {
                let mut file = File::open(&disk_path).with_context(|| {
                    format!("failed to open for archiving: `{}`", disk_path.display())
                })?;
                let metadata = file.metadata().with_context(|| {
                    format!("could not learn metadata for: `{}`", disk_path.display())
                })?;
                header.set_metadata_in_mode(&metadata, HeaderMode::Deterministic);
                header.set_cksum();
                ar.append_data(&mut header, &ar_path, &mut file)
                    .with_context(|| {
                        format!("could not archive source file `{}`", disk_path.display())
                    })?;
            }
            FileContents::Generated(generated_kind) => {
                let contents = match generated_kind {
                    GeneratedFile::Manifest => pkg.to_registry_toml(ws)?,
                    GeneratedFile::Lockfile => build_lock(ws, pkg)?,
                    GeneratedFile::VcsInfo(ref s) => serde_json::to_string_pretty(s)?,
                };
                header.set_entry_type(EntryType::file());
                header.set_mode(0o644);
                header.set_size(contents.len() as u64);
                // use something nonzero to avoid rust-lang/cargo#9512
                header.set_mtime(1);
                header.set_cksum();
                ar.append_data(&mut header, &ar_path, contents.as_bytes())
                    .with_context(|| format!("could not archive source file `{}`", rel_str))?;
            }
        }
    }

    let encoder = ar.into_inner()?;
    encoder.finish()?;
    Ok(())
}

/// Generate warnings when packaging Cargo.lock, and the resolve have changed.
fn compare_resolve(
    config: &Config,
    current_pkg: &Package,
    orig_resolve: &Resolve,
    new_resolve: &Resolve,
) -> CargoResult<()> {
    if config.shell().verbosity() != Verbosity::Verbose {
        return Ok(());
    }
    let new_set: BTreeSet<PackageId> = new_resolve.iter().collect();
    let orig_set: BTreeSet<PackageId> = orig_resolve.iter().collect();
    let added = new_set.difference(&orig_set);
    // Removed entries are ignored, this is used to quickly find hints for why
    // an entry changed.
    let removed: Vec<&PackageId> = orig_set.difference(&new_set).collect();
    for pkg_id in added {
        if pkg_id.name() == current_pkg.name() && pkg_id.version() == current_pkg.version() {
            // Skip the package that is being created, since its SourceId
            // (directory) changes.
            continue;
        }
        // Check for candidates where the source has changed (such as [patch]
        // or a dependency with multiple sources like path/version).
        let removed_candidates: Vec<&PackageId> = removed
            .iter()
            .filter(|orig_pkg_id| {
                orig_pkg_id.name() == pkg_id.name() && orig_pkg_id.version() == pkg_id.version()
            })
            .cloned()
            .collect();
        let extra = match removed_candidates.len() {
            0 => {
                // This can happen if the original was out of date.
                let previous_versions: Vec<&PackageId> = removed
                    .iter()
                    .filter(|orig_pkg_id| orig_pkg_id.name() == pkg_id.name())
                    .cloned()
                    .collect();
                match previous_versions.len() {
                    0 => String::new(),
                    1 => format!(
                        ", previous version was `{}`",
                        previous_versions[0].version()
                    ),
                    _ => format!(
                        ", previous versions were: {}",
                        previous_versions
                            .iter()
                            .map(|pkg_id| format!("`{}`", pkg_id.version()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
            1 => {
                // This can happen for multi-sourced dependencies like
                // `{path="...", version="..."}` or `[patch]` replacement.
                // `[replace]` is not captured in Cargo.lock.
                format!(
                    ", was originally sourced from `{}`",
                    removed_candidates[0].source_id()
                )
            }
            _ => {
                // I don't know if there is a way to actually trigger this,
                // but handle it just in case.
                let comma_list = removed_candidates
                    .iter()
                    .map(|pkg_id| format!("`{}`", pkg_id.source_id()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    ", was originally sourced from one of these sources: {}",
                    comma_list
                )
            }
        };
        let msg = format!(
            "package `{}` added to the packaged Cargo.lock file{}",
            pkg_id, extra
        );
        config.shell().note(msg)?;
    }
    Ok(())
}

fn check_yanked(config: &Config, pkg_set: &PackageSet<'_>, resolve: &Resolve) -> CargoResult<()> {
    // Checking the yanked status involves taking a look at the registry and
    // maybe updating files, so be sure to lock it here.
    let _lock = config.acquire_package_cache_lock()?;

    let mut sources = pkg_set.sources_mut();
    for pkg_id in resolve.iter() {
        if let Some(source) = sources.get_mut(pkg_id.source_id()) {
            if source.is_yanked(pkg_id)? {
                config.shell().warn(format!(
                    "package `{}` in Cargo.lock is yanked in registry `{}`, \
                     consider updating to a version that is not yanked",
                    pkg_id,
                    pkg_id.source_id().display_registry_name()
                ))?;
            }
        }
    }
    Ok(())
}

fn run_verify(
    ws: &Workspace<'_>,
    pkg: &Package,
    tar: &FileLock,
    opts: &PackageOpts<'_>,
) -> CargoResult<()> {
    let config = ws.config();

    config.shell().status("Verifying", pkg)?;

    let f = GzDecoder::new(tar.file());
    let dst = tar
        .parent()
        .join(&format!("{}-{}", pkg.name(), pkg.version()));
    if dst.exists() {
        paths::remove_dir_all(&dst)?;
    }
    let mut archive = Archive::new(f);
    // We don't need to set the Modified Time, as it's not relevant to verification
    // and it errors on filesystems that don't support setting a modified timestamp
    archive.set_preserve_mtime(false);
    archive.unpack(dst.parent().unwrap())?;

    // Manufacture an ephemeral workspace to ensure that even if the top-level
    // package has a workspace we can still build our new crate.
    let id = SourceId::for_path(&dst)?;
    let mut src = PathSource::new(&dst, id, ws.config());
    let new_pkg = src.root_package()?;
    let pkg_fingerprint = hash_all(&dst)?;
    let ws = Workspace::ephemeral(new_pkg, config, None, true)?;

    let rustc_args = if pkg
        .manifest()
        .unstable_features()
        .require(Feature::public_dependency())
        .is_ok()
    {
        // FIXME: Turn this on at some point in the future
        //Some(vec!["-D exported_private_dependencies".to_string()])
        Some(vec![])
    } else {
        None
    };

    let exec: Arc<dyn Executor> = Arc::new(DefaultExecutor);
    ops::compile_with_exec(
        &ws,
        &ops::CompileOptions {
            build_config: BuildConfig::new(config, opts.jobs, &opts.targets, CompileMode::Build)?,
            cli_features: opts.cli_features.clone(),
            spec: ops::Packages::Packages(Vec::new()),
            filter: ops::CompileFilter::Default {
                required_features_filterable: true,
            },
            target_rustdoc_args: None,
            target_rustc_args: rustc_args,
            local_rustdoc_args: None,
            rustdoc_document_private_items: false,
            honor_rust_version: true,
        },
        &exec,
    )?;

    // Check that `build.rs` didn't modify any files in the `src` directory.
    let ws_fingerprint = hash_all(&dst)?;
    if pkg_fingerprint != ws_fingerprint {
        let changes = report_hash_difference(&pkg_fingerprint, &ws_fingerprint);
        anyhow::bail!(
            "Source directory was modified by build.rs during cargo publish. \
             Build scripts should not modify anything outside of OUT_DIR.\n\
             {}\n\n\
             To proceed despite this, pass the `--no-verify` flag.",
            changes
        )
    }

    Ok(())
}

fn hash_all(path: &Path) -> CargoResult<HashMap<PathBuf, u64>> {
    fn wrap(path: &Path) -> CargoResult<HashMap<PathBuf, u64>> {
        let mut result = HashMap::new();
        let walker = walkdir::WalkDir::new(path).into_iter();
        for entry in walker.filter_entry(|e| !(e.depth() == 1 && e.file_name() == "target")) {
            let entry = entry?;
            let file_type = entry.file_type();
            if file_type.is_file() {
                let file = File::open(entry.path())?;
                let hash = util::hex::hash_u64_file(&file)?;
                result.insert(entry.path().to_path_buf(), hash);
            } else if file_type.is_symlink() {
                let hash = util::hex::hash_u64(&fs::read_link(entry.path())?);
                result.insert(entry.path().to_path_buf(), hash);
            } else if file_type.is_dir() {
                let hash = util::hex::hash_u64(&());
                result.insert(entry.path().to_path_buf(), hash);
            }
        }
        Ok(result)
    }
    let result = wrap(path).with_context(|| format!("failed to verify output at {:?}", path))?;
    Ok(result)
}

fn report_hash_difference(orig: &HashMap<PathBuf, u64>, after: &HashMap<PathBuf, u64>) -> String {
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    for (key, value) in orig {
        match after.get(key) {
            Some(after_value) => {
                if value != after_value {
                    changed.push(key.to_string_lossy());
                }
            }
            None => removed.push(key.to_string_lossy()),
        }
    }
    let mut added: Vec<_> = after
        .keys()
        .filter(|key| !orig.contains_key(*key))
        .map(|key| key.to_string_lossy())
        .collect();
    let mut result = Vec::new();
    if !changed.is_empty() {
        changed.sort_unstable();
        result.push(format!("Changed: {}", changed.join("\n\t")));
    }
    if !added.is_empty() {
        added.sort_unstable();
        result.push(format!("Added: {}", added.join("\n\t")));
    }
    if !removed.is_empty() {
        removed.sort_unstable();
        result.push(format!("Removed: {}", removed.join("\n\t")));
    }
    assert!(!result.is_empty(), "unexpected empty change detection");
    result.join("\n")
}

// It can often be the case that files of a particular name on one platform
// can't actually be created on another platform. For example files with colons
// in the name are allowed on Unix but not on Windows.
//
// To help out in situations like this, issue about weird filenames when
// packaging as a "heads up" that something may not work on other platforms.
fn check_filename(file: &Path, shell: &mut Shell) -> CargoResult<()> {
    let name = match file.file_name() {
        Some(name) => name,
        None => return Ok(()),
    };
    let name = match name.to_str() {
        Some(name) => name,
        None => anyhow::bail!(
            "path does not have a unicode filename which may not unpack \
             on all platforms: {}",
            file.display()
        ),
    };
    let bad_chars = ['/', '\\', '<', '>', ':', '"', '|', '?', '*'];
    if let Some(c) = bad_chars.iter().find(|c| name.contains(**c)) {
        anyhow::bail!(
            "cannot package a filename with a special character `{}`: {}",
            c,
            file.display()
        )
    }
    if restricted_names::is_windows_reserved_path(file) {
        shell.warn(format!(
            "file {} is a reserved Windows filename, \
                it will not work on Windows platforms",
            file.display()
        ))?;
    }
    Ok(())
}
