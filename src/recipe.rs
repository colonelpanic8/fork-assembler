//! Recipes: what one commit of the maintenance repository, or its working
//! tree, says to build — and the assembled commit that recipe produced.
//!
//! Generated commits are deterministic, so the commit a lock records can
//! always be rebuilt from that lock's own recipe. Nothing needs preserving for
//! the assembled branch of any past maintenance commit to be at hand: when the
//! object is gone, the maintenance commit regenerates it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::engine::{self, Ctx};
use crate::git;
use crate::lock::{self, BuildRecord, Lock};
use crate::manifest::{self, Manifest};
use crate::report::{Event, Report, Silent};
use crate::state;

/// Where the source repository caches each maintenance commit's assembled
/// commit: `ASSEMBLED` + the maintenance commit's full OID. Only a cache —
/// it spares a later lookup the regeneration.
pub const ASSEMBLED: &str = "refs/fork-assembler/assembled/";

/// A maintenance commit's recipe, checked out on its own; or the working
/// tree's, which is the maintenance repository itself.
pub struct Recipe {
    /// The maintenance commit; None for the working tree.
    pub rev: Option<String>,
    /// Where the recipe's manifest, lock, pairs and patches are.
    pub root: PathBuf,
    pub manifest: Manifest,
    pub lock: Lock,
    _checkout: Option<Checkout>,
}

/// A detached worktree of the maintenance repository, removed when dropped.
struct Checkout {
    repo: PathBuf,
    path: PathBuf,
}

impl Drop for Checkout {
    fn drop(&mut self) {
        let path = self.path.to_string_lossy();
        let _ = git::raw(&self.repo, &["worktree", "remove", "--force", &path]);
        let _ = git::raw(&self.repo, &["worktree", "prune"]);
    }
}

/// How an assembled commit came to be present.
#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The object was already in the source repository.
    Present,
    /// Rebuilt from the recipe, and verified against its lock.
    Regenerated,
}

/// A recipe's assembled result, present in the source repository.
#[derive(Serialize, Clone)]
pub struct Assembled {
    pub commit: String,
    /// The content commit beneath the provenance record; what comparisons use,
    /// since the provenance record changes with every recipe.
    pub pre_provenance_commit: String,
    pub base: String,
    pub source: Source,
}

/// Resolve `rev` to a full commit OID in the maintenance repository at `root`.
pub fn resolve(root: &Path, rev: &str) -> Result<String> {
    let spec = format!("{rev}^{{commit}}");
    git::out(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ],
    )
    .with_context(|| format!("{rev:?} is not a commit of the maintenance repository"))
}

impl Recipe {
    /// The recipe at maintenance commit `rev`.
    pub fn at(root: &Path, rev: &str) -> Result<Recipe> {
        let oid = resolve(root, rev)?;
        let repo = PathBuf::from(git::out(root, &["rev-parse", "--show-toplevel"])?);
        let prefix = git::out(root, &["rev-parse", "--show-prefix"])?;
        let common = PathBuf::from(git::out(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?);
        static CHECKOUTS: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "{}-{}-{}",
            git::short(&oid),
            std::process::id(),
            CHECKOUTS.fetch_add(1, Ordering::Relaxed)
        );
        let path = common.join("fork-assembler").join("recipes").join(name);
        let _ = git::raw(&repo, &["worktree", "prune"]);
        git::out(
            &repo,
            &["worktree", "add", "--detach", &path.to_string_lossy(), &oid],
        )?;
        let checkout = Checkout {
            repo,
            path: path.clone(),
        };
        let root = path.join(prefix);
        let manifest = manifest::load(&root)
            .with_context(|| format!("reading the manifest at {}", git::short(&oid)))?;
        let lock = lock::load(&root)?.unwrap_or_default();
        Ok(Recipe {
            rev: Some(oid),
            root,
            manifest,
            lock,
            _checkout: Some(checkout),
        })
    }

    /// The recipe in the working tree at `root`.
    pub fn working(root: &Path) -> Result<Recipe> {
        Ok(Recipe {
            rev: None,
            root: root.to_path_buf(),
            manifest: manifest::load(root)?,
            lock: lock::load(root)?.unwrap_or_default(),
            _checkout: None,
        })
    }

    /// How reports name this recipe.
    pub fn label(&self) -> String {
        match &self.rev {
            Some(rev) => git::short(rev).to_string(),
            None => "working tree".to_string(),
        }
    }

    pub fn build(&self) -> Result<&BuildRecord> {
        self.lock
            .build
            .as_ref()
            .with_context(|| format!("{}: the lock records no completed build", self.label()))
    }

    /// The assembled commit this recipe's lock records, made present in the
    /// source repository `repo`: found there, or regenerated from the recipe.
    /// `main_root` is the maintenance repository, whose in-progress build a
    /// regeneration must not disturb.
    pub fn assembled(
        &self,
        main_root: &Path,
        repo: &Path,
        report: &dyn Report,
    ) -> Result<Assembled> {
        let build = self.build()?;
        let source = if git::has_commit(repo, &build.commit) {
            Source::Present
        } else {
            self.regenerate(main_root, repo, build, report)?;
            Source::Regenerated
        };
        Ok(Assembled {
            commit: build.commit.clone(),
            pre_provenance_commit: build.pre_provenance_commit.clone(),
            base: build.base.clone(),
            source,
        })
    }

    fn regenerate(
        &self,
        main_root: &Path,
        repo: &Path,
        build: &BuildRecord,
        report: &dyn Report,
    ) -> Result<()> {
        let label = self.label();
        let commit = git::short(&build.commit);
        if self.rev.is_none() {
            bail!(
                "the working tree's assembled commit {commit} is not in the source repository; \
                 run `fork-assembler build` to make it"
            );
        }
        if build.protocol != lock::PROTOCOL {
            bail!(
                "{label}: its assembled commit {commit} is gone from the source repository, and \
                 the build that made it predates deterministic commits, so it cannot be \
                 regenerated"
            );
        }
        let main_build = main_root.join(engine::WORKTREE);
        if state::in_progress(&main_build) {
            bail!(
                "a build is in progress in {}, and regenerating {label} would reseed the rerere \
                 cache it shares; finish it with `fork-assembler continue` first",
                main_build.display()
            );
        }
        report.event(&Event::Regenerating {
            recipe: label.clone(),
            commit: build.commit.clone(),
        });
        self.fetch_missing(repo);
        let ctx = Ctx::new(
            &self.root,
            self.manifest.clone(),
            repo.to_path_buf(),
            &Silent,
        );
        let head = engine::reproduce(&ctx).with_context(|| format!("regenerating {label}"))?;
        if head != build.commit {
            bail!(
                "regenerating {label} produced {head}, not the {} its lock records",
                build.commit
            );
        }
        Ok(())
    }

    /// Fetch, by OID, every pinned object the source repository has lost.
    /// Holding refs move on with each update, so an old recipe's pins are
    /// eventually unreachable locally and pruned, while remotes usually still
    /// serve them by OID. Failures are left for the build to report: it names
    /// precisely what is missing.
    fn fetch_missing(&self, repo: &Path) {
        let m = &self.manifest;
        let pins = &self.lock.pins;
        let mut wanted: BTreeMap<String, &str> = BTreeMap::new();
        if let Some(base) = &pins.base {
            wanted.insert(base.clone(), m.base.remote.as_str());
        }
        for entry in &m.entries {
            if let (Some((remote, _)), Some(pin)) =
                (entry.kind.remote_ref(), pins.entries.get(&entry.name))
            {
                wanted.insert(pin.clone(), remote);
            }
            for parent in &entry.parents {
                let pin = pins
                    .parents
                    .get(&entry.name)
                    .and_then(|p| p.get(&parent.name));
                if let (Some((remote, _)), Some(pin)) = (parent.kind.remote_ref(), pin) {
                    wanted.insert(pin.clone(), remote);
                }
            }
        }
        for (oid, remote) in wanted {
            if git::has_commit(repo, &oid) {
                continue;
            }
            if let Ok(url) = m.remote_url(remote) {
                let _ = git::transport(repo, &["fetch", "--no-tags", url, &oid]);
            }
        }
    }

    /// Cache `commit` as what this recipe's maintenance commit built.
    pub fn record(&self, repo: &Path, commit: &str) -> Result<()> {
        if let Some(rev) = &self.rev {
            git::out(repo, &["update-ref", &format!("{ASSEMBLED}{rev}"), commit])?;
        }
        Ok(())
    }
}
