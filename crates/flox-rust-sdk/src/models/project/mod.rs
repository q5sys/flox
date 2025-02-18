use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use log::{debug, info};
use once_cell::sync::Lazy;
use regex::Regex;
use runix::arguments::{EvalArgs, NixArgs};
use runix::command::{Eval, FlakeInit};
use runix::installable::Installable;
use runix::{NixBackend, Run, RunJson};
use tempfile::TempDir;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use walkdir::WalkDir;

use self::environment::Environment;
use super::root::transaction::{GitAccess, GitSandBox, ReadOnly};
use super::root::{Closed, Root};
use crate::flox::{Flox, FloxNixApi};
use crate::providers::git::GitProvider;
use crate::utils::errors::IoError;
use crate::utils::guard::Guard;
use crate::utils::{copy_file_without_permissions, find_and_replace, FindAndReplaceError};

pub mod environment;

static PNAME_DECLARATION: Lazy<Regex> = Lazy::new(|| Regex::new(r#"pname = ".*""#).unwrap());
static PACKAGE_NAME_PLACEHOLDER: &str = "__PACKAGE_NAME__";

#[derive(Debug)]
/// A representation of a project, i.e. a git repo with a flake.nix
///
/// We assume the flake.nix follows the capacitor output schema
pub struct Project<'flox, Git: GitProvider, Access: GitAccess<Git>> {
    flox: &'flox Flox,
    git: Access,
    /// subdir relative to the git workdir
    ///
    /// Represent setups where the project is not in the git root,
    /// or is a subflake.
    /// One such places are named env's generations:
    ///
    /// ```ignore
    /// /
    /// L .git/
    /// L 1/
    ///   L flake.nix
    ///   L pkgs/
    ///     L default/
    ///       L flox.nix
    /// ```
    subdir: PathBuf,
    _marker: PhantomData<Git>,
}

/// Upgrade paths from a git repo into an open Project
impl<'flox, Git: GitProvider> Root<'flox, Closed<Git>> {
    /// Guards opening a project
    ///
    /// - Resolves as initialized if a `flake.nix` is present
    /// - Resolves as uninitialized if not
    pub async fn guard(
        self,
    ) -> Result<Guard<Project<'flox, Git, ReadOnly<Git>>, Root<'flox, Closed<Git>>>, OpenProjectError>
    {
        let repo = &self.state.inner;

        let root = repo.workdir().ok_or(OpenProjectError::WorkdirNotFound)?;

        // todo: inset
        if root.join("flake.nix").exists() {
            Ok(Guard::Initialized(Project::new(
                self.flox,
                ReadOnly::new(self.state.inner),
                PathBuf::new(),
            )))
        } else {
            Ok(Guard::Uninitialized(self))
        }
    }
}

/// Resolutions for unsucessful upgrades
impl<'flox, Git: GitProvider> Guard<Project<'flox, Git, ReadOnly<Git>>, Root<'flox, Closed<Git>>> {
    /// Initialize a new project in the workdir of a git root or return
    /// an existing project if it exists.
    pub async fn init_project<Nix: FloxNixApi>(
        self,
        nix_extra_args: Vec<String>,
    ) -> Result<Project<'flox, Git, ReadOnly<Git>>, InitProjectError<Nix, Git>>
    where
        FlakeInit: Run<Nix>,
    {
        if let Guard::Initialized(i) = self {
            return Ok(i);
        }

        let uninit = match self {
            Guard::Uninitialized(u) => u,
            _ => unreachable!(), // returned above
        };

        let repo = uninit.state.inner;

        let root = repo
            .workdir()
            .ok_or(InitProjectError::<Nix, Git>::WorkdirNotFound)?;

        let nix = uninit.flox.nix(nix_extra_args);

        FlakeInit {
            template: Some("flox#templates._init".to_string().into()),
            ..Default::default()
        }
        .run(&nix, &NixArgs {
            cwd: Some(root.to_path_buf()),
            ..Default::default()
        })
        .await
        .map_err(InitProjectError::NixInitBase)?;

        repo.add(&[Path::new("flake.nix")])
            .await
            .map_err(InitProjectError::GitAdd)?;

        Ok(Project::new(
            uninit.flox,
            ReadOnly::new(repo),
            PathBuf::new(),
        ))
    }
}

/// Implementations for an opened project (read only)
impl<'flox, Git: GitProvider, Access: GitAccess<Git>> Project<'flox, Git, Access> {
    /// Construct a new Project object
    ///
    /// Private in this module, as intialization through git guard is prefered
    /// to provide project guarantees.
    fn new(flox: &Flox, git: Access, subdir: PathBuf) -> Project<Git, Access> {
        Project {
            flox,
            git,
            subdir,
            _marker: PhantomData,
        }
    }

    /// Get the root directory of the project flake
    ///
    /// currently the git root but may be a subdir with a flake.nix
    pub fn workdir(&self) -> Option<&Path> {
        self.git.git().workdir()
    }

    /// flakeref for the project
    // todo: use typed FlakeRefs
    pub fn flakeref(&self) -> String {
        self.workdir().unwrap().to_string_lossy().to_string()
    }

    /// Add a new flox style package from a template.
    /// Uses `nix flake init` to retrieve files
    /// and postprocesses the generic templates.
    //
    // todo: move to mutable state
    pub async fn init_flox_package<Nix: FloxNixApi>(
        &self,
        nix_extra_args: Vec<String>,
        template: Installable,
        name: &str,
    ) -> Result<(), InitFloxPackageError<Nix, Git>>
    where
        FlakeInit: Run<Nix>,
    {
        let repo = self.git.git();

        let nix = self.flox.nix(nix_extra_args);

        let root = repo
            .workdir()
            .ok_or(InitFloxPackageError::WorkdirNotFound)?;

        FlakeInit {
            template: Some(template.to_string().into()),
            ..Default::default()
        }
        .run(&nix, &NixArgs {
            cwd: root.to_path_buf().into(),
            ..NixArgs::default()
        })
        .await
        .map_err(InitFloxPackageError::NixInit)?;

        let old_package_path = root.join("pkgs/default.nix");

        match tokio::fs::File::open(&old_package_path).await {
            // legacy path. Drop after we merge template changes to floxpkgs
            Ok(mut file) => {
                let mut package_contents = String::new();
                file.read_to_string(&mut package_contents)
                    .await
                    .map_err(InitFloxPackageError::ReadTemplateFile)?;

                // Drop handler should clear our file handle in case we want to delete it
                drop(file);

                let new_contents =
                    PNAME_DECLARATION.replace(&package_contents, format!(r#"pname = "{name}""#));

                let new_package_dir = root.join("pkgs").join(name);
                debug!("creating dir: {}", new_package_dir.display());
                tokio::fs::create_dir_all(&new_package_dir)
                    .await
                    .map_err(InitFloxPackageError::MkNamedDir)?;

                let new_package_path = new_package_dir.join("default.nix");

                repo.rm(&[&old_package_path], false, true, false)
                    .await
                    .map_err(InitFloxPackageError::RemoveUnnamedFile)?;

                let mut file = tokio::fs::File::create(&new_package_path)
                    .await
                    .map_err(InitFloxPackageError::OpenNamed)?;

                file.write_all(new_contents.as_bytes())
                    .await
                    .map_err(InitFloxPackageError::WriteTemplateFile)?;

                repo.add(&[&new_package_path])
                    .await
                    .map_err(InitFloxPackageError::GitAdd)?;

                // this might technically be a lie, but it's close enough :)
                info!("renamed: pkgs/default.nix -> pkgs/{name}/default.nix");
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => 'move_to_pkgs: {
                let old_proto_pkg_path = root.join("pkgs").join(PACKAGE_NAME_PLACEHOLDER);

                if !old_proto_pkg_path.exists() {
                    // TODO: really find a better way to not hardcode this
                    if template.to_string() == "flake:flox#.\"templates\".\"project\"" {
                        repo.add(&[&root.join("flox.nix")])
                            .await
                            .map_err(InitFloxPackageError::GitAdd)?;
                    }

                    break 'move_to_pkgs;
                }

                let new_proto_pkg_path = root.join("pkgs").join(name);

                repo.mv(&old_proto_pkg_path, &new_proto_pkg_path)
                    .await
                    .map_err(InitFloxPackageError::GitMv)?;
                info!(
                    "moved: {} -> {}",
                    old_proto_pkg_path.to_string_lossy(),
                    new_proto_pkg_path.to_string_lossy()
                );

                // our minimal "templating" - Replace any occurrences of
                // PACKAGE_NAME_PLACEHOLDER with name
                find_and_replace(&new_proto_pkg_path, PACKAGE_NAME_PLACEHOLDER, name)
                    .await
                    .map_err(InitFloxPackageError::<Nix, Git>::ReplacePackageName)?;

                repo.add(&[&new_proto_pkg_path])
                    .await
                    .map_err(InitFloxPackageError::GitAdd)?;
            },
            Err(err) => return Err(InitFloxPackageError::OpenTemplateFile(err)),
        };
        Ok(())
    }

    /// Delete flox files from repo
    pub async fn cleanup_flox(self) -> Result<(), CleanupInitializerError> {
        tokio::fs::remove_dir_all("./pkgs")
            .await
            .map_err(CleanupInitializerError::RemovePkgs)?;
        tokio::fs::remove_file("./flake.nix")
            .await
            .map_err(CleanupInitializerError::RemoveFlake)?;

        Ok(())
    }

    /// Get a particular environment by name
    /// (attr path once nested packages are implemented)
    pub async fn environment<Nix: FloxNixApi>(
        &self,
        name: &str,
    ) -> Result<Environment<'flox, Git, ReadOnly<Git>>, ()>
    where
        Eval: RunJson<Nix>,
    {
        let nix = self.flox.nix::<Nix>(Default::default());

        let nix_apply_expr = format!(
            r#"systems: (systems."{}" or {{}}) ? "{name}""#,
            self.flox.system
        );

        let eval = Eval {
            eval_args: EvalArgs {
                apply: Some(nix_apply_expr.into()),
                installable: Some(Installable::new(self.flakeref(), "floxEnvs".to_string()).into()),
            },
            ..Eval::default()
        };

        let env = eval.run_json(&nix, &Default::default()).await.unwrap();
        let env = serde_json::from_value::<bool>(env).unwrap();

        env.then(|| Environment {
            name: name.to_string(),
            system: self.flox.system.clone(),
            project: Project::new(self.flox, self.git.read_only(), self.subdir.clone()),
        })
        .ok_or(())
    }

    /// List environments in this project
    pub async fn environments<Nix: FloxNixApi>(
        &'flox self,
    ) -> Result<Vec<Environment<'flox, Git, ReadOnly<Git>>>, GetEnvironmentsError<Nix>>
    where
        Eval: RunJson<Nix>,
    {
        let nix = self.flox.nix::<Nix>(Default::default());

        let nix_apply_expr = format!(
            r#"systems: builtins.attrNames (systems."{}" or {{}})"#,
            self.flox.system
        );

        let eval = Eval {
            eval_args: EvalArgs {
                apply: Some(nix_apply_expr.into()),
                installable: Some(Installable::new(self.flakeref(), "floxEnvs".to_string()).into()),
            },
            ..Eval::default()
        };

        let names = eval.run_json(&nix, &Default::default()).await.unwrap();
        let names = serde_json::from_value::<Vec<String>>(names).unwrap();

        let envs = names
            .into_iter()
            .map(|name| Environment {
                name,
                system: self.flox.system.clone(),
                project: Project::new(self.flox, self.git.read_only(), self.subdir.clone()),
            })
            .collect();

        Ok(envs)
    }
}

/// Implementations exclusively for [ReadOnly] instances
impl<'flox, Git: GitProvider> Project<'flox, Git, ReadOnly<Git>> {
    pub async fn enter_transaction(
        self,
    ) -> Result<(Project<'flox, Git, GitSandBox<Git>>, Index), TransactionEnterError> {
        let transaction_temp_dir =
            TempDir::new_in(&self.flox.temp_dir).map_err(TransactionEnterError::CreateTempdir)?;

        let current_root = self.workdir().expect("only supports projects on FS");

        for entry in WalkDir::new(current_root).into_iter().skip(1) {
            let entry = entry.map_err(TransactionEnterError::Walkdir)?;
            let new_path = transaction_temp_dir
                .path()
                .join(entry.path().strip_prefix(current_root).unwrap());
            if entry.file_type().is_dir() {
                tokio::fs::create_dir(new_path)
                    .await
                    .map_err(TransactionEnterError::CopyDir)?;
            } else {
                copy_file_without_permissions(entry.path(), &new_path)
                    .await
                    .map_err(TransactionEnterError::CopyFile)?;
            }
        }

        let git = Git::discover(transaction_temp_dir.path()).await.unwrap();

        let sandbox = self.git.to_sandbox_in(transaction_temp_dir, git);

        Ok((
            Project {
                flox: self.flox,
                git: sandbox,
                subdir: self.subdir,
                _marker: PhantomData,
            },
            Index::default(),
        ))
    }
}

type Index = BTreeMap<PathBuf, FileAction>;
pub enum FileAction {
    Add,
    Delete,
}

/// Implementations exclusively for [GitSandBox]ed instances
impl<'flox, Git: GitProvider> Project<'flox, Git, GitSandBox<Git>> {
    pub async fn commit_transaction(
        self,
        index: Index,
        _message: &str,
    ) -> Result<Project<'flox, Git, ReadOnly<Git>>, TransactionCommitError<Git>> {
        let original = self.git.read_only();

        for (file, action) in index {
            match action {
                FileAction::Add => {
                    if let Some(parent) = file.parent() {
                        tokio::fs::create_dir_all(original.git().workdir().unwrap().join(parent))
                            .await
                            .unwrap();
                    }
                    tokio::fs::rename(
                        self.git.git().workdir().unwrap().join(&file),
                        original.git().workdir().unwrap().join(&file),
                    )
                    .await
                    .unwrap();

                    original.git().add(&[&file]).await.expect("should add file")
                },
                FileAction::Delete => {
                    original
                        .git()
                        .rm(
                            &[&file],
                            original.git().workdir().unwrap().join(&file).is_dir(),
                            false,
                            false,
                        )
                        .await
                        .expect("should remove path");
                },
            }
        }

        Ok(Project {
            flox: self.flox,
            git: original,
            subdir: self.subdir,
            _marker: PhantomData,
        })
    }

    /// create a new root
    pub async fn create_default_env(&self, index: &mut Index) {
        let path = Path::new("flox.nix").to_path_buf();
        tokio::fs::write(
            self.workdir().expect("only works with workdir").join(&path),
            include_str!("./flox.nix.in"),
        )
        .await
        .unwrap();
        index.insert(path, FileAction::Add);
    }
}

#[derive(Error, Debug)]
pub enum TransactionEnterError {
    #[error("Failed to create tempdir for transaction")]
    CreateTempdir(std::io::Error),
    #[error("Failed to walk over file: {0}")]
    Walkdir(walkdir::Error),
    #[error("Failed to copy dir")]
    CopyDir(std::io::Error),
    #[error("Failed to copy file")]
    CopyFile(IoError),
}
#[derive(Error, Debug)]
pub enum TransactionCommitError<Git: GitProvider> {
    GitCommit(Git::CommitError),
    GitPush(Git::PushError),
}

/// Errors occurring while trying to upgrade to an [`Open<Git>`] [Root]
#[derive(Error, Debug)]
pub enum OpenProjectError {
    #[error("Could not determine repository root")]
    WorkdirNotFound,
}

#[derive(Error, Debug)]
pub enum InitProjectError<Nix: NixBackend, Git: GitProvider>
where
    FlakeInit: Run<Nix>,
{
    #[error("Could not determine repository root")]
    WorkdirNotFound,

    #[error("Error initializing base template with Nix")]
    NixInitBase(<FlakeInit as Run<Nix>>::Error),
    #[error("Error reading template file contents")]
    ReadTemplateFile(std::io::Error),
    #[error("Error truncating template file")]
    TruncateTemplateFile(std::io::Error),
    #[error("Error writing to template file")]
    WriteTemplateFile(std::io::Error),
    #[error("Error new template file in Git")]
    GitAdd(Git::AddError),
}

#[derive(Error, Debug)]
pub enum InitFloxPackageError<Nix: NixBackend, Git: GitProvider>
where
    FlakeInit: Run<Nix>,
{
    #[error("Could not determine repository root")]
    WorkdirNotFound,
    #[error("Error initializing template with Nix")]
    NixInit(<FlakeInit as Run<Nix>>::Error),
    #[error("Error moving template file to named location using Git")]
    MvNamed(Git::MvError),
    #[error("Error opening template file")]
    OpenTemplateFile(std::io::Error),
    #[error("Error reading template file contents")]
    ReadTemplateFile(std::io::Error),
    #[error("Error truncating template file")]
    TruncateTemplateFile(std::io::Error),
    #[error("Error writing to template file")]
    WriteTemplateFile(std::io::Error),
    #[error("Error making named directory")]
    MkNamedDir(std::io::Error),
    #[error("Error opening new renamed file for writing")]
    OpenNamed(std::io::Error),
    #[error("Error removing old unnamed file using Git")]
    RemoveUnnamedFile(Git::RmError),
    #[error("Error staging new renamed file in Git")]
    GitAdd(Git::AddError),
    #[error("Error moving file in Git")]
    GitMv(Git::MvError),
    #[error("Error replacing {}: {0}", PACKAGE_NAME_PLACEHOLDER)]
    ReplacePackageName(FindAndReplaceError),
}

#[derive(Error, Debug)]
pub enum CleanupInitializerError {
    #[error("Error removing pkgs")]
    RemovePkgs(std::io::Error),
    #[error("Error removing flake.nix")]
    RemoveFlake(std::io::Error),
}

#[derive(Error, Debug)]
pub enum GetEnvironmentsError<Nix: NixBackend>
where
    Eval: RunJson<Nix>,
{
    ListEnvironments(<Eval as RunJson<Nix>>::JsonError),
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;
    use crate::prelude::ChannelRegistry;
    use crate::providers::git::GitCommandProvider;

    fn flox_instance() -> (Flox, TempDir) {
        let tempdir_handle = tempfile::tempdir_in(std::env::temp_dir()).unwrap();

        let cache_dir = tempdir_handle.path().join("caches");
        let temp_dir = tempdir_handle.path().join("temp");
        let config_dir = tempdir_handle.path().join("config");

        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&temp_dir).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();

        let mut channels = ChannelRegistry::default();
        channels.register_channel("flox", "github:flox/floxpkgs/master".parse().unwrap());

        let flox = Flox {
            system: "aarch64-darwin".to_string(),
            cache_dir,
            temp_dir,
            config_dir,
            channels,
            ..Default::default()
        };

        (flox, tempdir_handle)
    }

    #[tokio::test]
    async fn fail_without_git() {
        let (flox, tempdir_handle) = flox_instance();

        let project_dir = tempfile::tempdir_in(tempdir_handle.path()).unwrap();

        flox.resource(project_dir.path().to_path_buf())
            .guard::<GitCommandProvider>()
            .await
            .expect("Finding dir should succeed")
            .open()
            .expect_err("should find empty dir");
    }

    #[tokio::test]
    async fn fail_without_flake_nix() {
        let (flox, tempdir_handle) = flox_instance();

        let project_dir = tempfile::tempdir_in(tempdir_handle.path()).unwrap();
        let _project_git = GitCommandProvider::init(project_dir.path(), false)
            .await
            .expect("should create git repo");

        flox.resource(project_dir.path().to_path_buf())
            .guard::<GitCommandProvider>()
            .await
            .expect("Finding dir should succeed")
            .open()
            .expect("should find git repo")
            .guard()
            .await
            .expect("Openeing project dir should succeed")
            .open()
            .expect_err("Should error without flake.nix");
    }

    #[cfg(feature = "impure-unit-tests")]
    #[tokio::test]
    async fn create_project() {
        let temp_home = tempfile::tempdir().unwrap();
        env::set_var("HOME", temp_home.path());

        let (flox, tempdir_handle) = flox_instance();

        let project_dir = tempfile::tempdir_in(tempdir_handle.path()).unwrap();
        let _project_git = GitCommandProvider::init(project_dir.path(), false)
            .await
            .expect("should create git repo");

        let project = flox
            .resource(project_dir.path().to_path_buf())
            .guard::<GitCommandProvider>()
            .await
            .expect("Finding dir should succeed")
            .open()
            .expect("should find git repo")
            .guard()
            .await
            .expect("Openeing project dir should succeed")
            .init_project(Vec::new())
            .await
            .expect("Should init a new project");

        let envs = project
            .environments()
            .await
            .expect("should find empty floxEnvs");
        assert!(envs.is_empty());

        let (project, mut index) = project
            .enter_transaction()
            .await
            .expect("Should be able to make sandbox");

        project.create_default_env(&mut index).await;

        let project = project
            .commit_transaction(index, "unused")
            .await
            .expect("Should commit transaction");

        project
            .environment("default")
            .await
            .expect("should find new environment");
    }
}
