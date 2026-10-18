//! Fetching live refs and resolving the pins a build consumes.
//!
//! `build` never moves an existing pin (`update` is the only verb that does);
//! entries with no pin yet get pinned from live refs on their first build.
//! Parents obey the same rule.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::Ctx;
use crate::git;
use crate::lock;
use crate::manifest::{self, Entry, Kind, Parent};
use crate::report::Event;

/// A private ref namespace so fetched heads never collide with user refs.
fn holding_ref(entry: &Entry) -> String {
    format!(
        "refs/fork-assembler/{}",
        manifest::sanitize_name(&entry.name)
    )
}

/// The base remote's tags, so provenance can place the base relative to
/// upstream releases without consulting whatever tags happen to be local.
pub const BASE_TAGS: &str = "refs/fork-assembler/base-tags/";

/// Parents get their own namespace under the entry that declares them: two
/// entries may legitimately declare the same parent, and each reconstruction
/// is standalone, so the holding refs must not be shared between them.
fn parent_holding_ref(entry: &Entry, parent: &Parent) -> String {
    format!(
        "refs/fork-assembler/parents/{}/{}",
        manifest::sanitize_name(&entry.name),
        manifest::sanitize_name(&parent.name)
    )
}

/// Where a reconstructed tip is parked. Nothing reads it back — the lock
/// records the OID — but it keeps the reconstruction reachable for the rest of
/// the build and leaves it inspectable afterwards, once the derive worktree is
/// gone.
pub fn derived_ref(entry: &Entry) -> String {
    format!(
        "refs/fork-assembler/derived/{}",
        manifest::sanitize_name(&entry.name)
    )
}

pub fn patch_blob(root: &Path, rel: &str) -> Result<String> {
    let path = root.join(rel);
    if !path.exists() {
        bail!("patch file {rel} does not exist");
    }
    git::out(root, &["hash-object", &path.to_string_lossy()])
}

/// Blob hash per entry carrying a coherence fixup, for lock snapshots.
/// `strict` fails on a missing file (what `build` wants); otherwise the entry
/// is left out of the map (what `status` wants, so it can still report).
pub fn fixup_blobs(
    root: &Path,
    entries: &[Entry],
    strict: bool,
) -> Result<BTreeMap<String, String>> {
    let mut blobs = BTreeMap::new();
    for entry in entries {
        let Some(rel) = &entry.fixup else { continue };
        match patch_blob(root, rel) {
            Ok(blob) => {
                blobs.insert(entry.name.clone(), blob);
            }
            Err(err) if strict => {
                return Err(err.context(format!("{}: coherence fixup", entry.name)))
            }
            Err(_) => {}
        }
    }
    Ok(blobs)
}

/// One live ref a verb needs fetched.
#[derive(Clone, Copy)]
pub enum Want<'a> {
    Base,
    /// A patch entry resolves to its file's blob; nothing is fetched.
    Entry(&'a Entry),
    Parent(&'a Entry, &'a Parent),
}

/// What a batch of `Want`s resolved to.
#[derive(Default)]
pub struct Live {
    base: Option<String>,
    entries: BTreeMap<String, String>,
    parents: BTreeMap<(String, String), String>,
}

impl Live {
    pub fn base(&self) -> Result<String> {
        self.base.clone().context("the base was not fetched")
    }

    pub fn entry(&self, entry: &Entry) -> Result<String> {
        self.entries
            .get(&entry.name)
            .cloned()
            .with_context(|| format!("{} was not fetched", entry.name))
    }

    pub fn parent(&self, entry: &Entry, parent: &Parent) -> Result<String> {
        self.parents
            .get(&(entry.name.clone(), parent.name.clone()))
            .cloned()
            .with_context(|| format!("{}: parent {} was not fetched", entry.name, parent.name))
    }
}

/// A `Want` that needs the network: one refspec on one remote.
struct RefFetch<'a> {
    want: Want<'a>,
    remote: &'a str,
    source: String,
    holding: String,
}

impl RefFetch<'_> {
    fn spec(&self) -> String {
        format!("+{}:{}", self.source, self.holding)
    }

    fn what(&self) -> String {
        match self.want {
            Want::Base => "base".to_string(),
            Want::Entry(entry) => entry.name.clone(),
            Want::Parent(entry, parent) => format!("{}: parent {}", entry.name, parent.name),
        }
    }
}

fn fetch_args<'s>(remote: &'s str, specs: &'s [String]) -> Vec<&'s str> {
    let mut args = vec!["fetch", "--no-tags", remote];
    args.extend(specs.iter().map(String::as_str));
    args
}

impl Ctx<'_> {
    fn ref_fetch<'a>(&'a self, want: Want<'a>) -> Option<RefFetch<'a>> {
        let (remote, source, holding) = match want {
            Want::Base => {
                let base = &self.manifest.base;
                (
                    base.remote.as_str(),
                    format!("refs/heads/{}", base.ref_),
                    "refs/fork-assembler/base".to_string(),
                )
            }
            Want::Entry(entry) => {
                let (remote, source) = entry.kind.remote_ref()?;
                (remote, source, holding_ref(entry))
            }
            Want::Parent(entry, parent) => {
                let (remote, source) = parent.kind.remote_ref()?;
                (remote, source, parent_holding_ref(entry, parent))
            }
        };
        Some(RefFetch {
            want,
            remote,
            source,
            holding,
        })
    }

    /// Fetch every wanted live ref into its holding ref with one `git fetch`
    /// per remote. Git aborts a whole fetch when any one ref is missing, so a
    /// failed batch is retried ref by ref to name the entry that broke it.
    pub fn fetch_live(&self, wants: &[Want]) -> Result<Live> {
        let mut live = Live::default();
        let mut by_remote: BTreeMap<&str, Vec<RefFetch>> = BTreeMap::new();
        for &want in wants {
            match (want, self.ref_fetch(want)) {
                (_, Some(fetch)) => by_remote.entry(fetch.remote).or_default().push(fetch),
                (Want::Entry(entry), None) => {
                    if let Kind::Patch { path } = &entry.kind {
                        live.entries
                            .insert(entry.name.clone(), patch_blob(&self.root, path)?);
                    }
                }
                (_, None) => bail!("a parent must be a fetchable ref, not a patch"),
            }
        }

        let base_wanted = wants.iter().any(|want| matches!(want, Want::Base));
        for (remote, fetches) in &by_remote {
            let mut specs: Vec<String> = fetches.iter().map(RefFetch::spec).collect();
            let tags = (base_wanted && *remote == self.manifest.base.remote)
                .then(|| format!("+refs/tags/*:{BASE_TAGS}*"));
            specs.extend(tags.clone());
            if git::out(&self.repo, &fetch_args(remote, &specs)).is_err() {
                for (fetch, spec) in fetches.iter().zip(&specs) {
                    git::out(&self.repo, &fetch_args(remote, std::slice::from_ref(spec)))
                        .with_context(|| {
                            format!("{}: fetching {remote} {}", fetch.what(), fetch.source)
                        })?;
                }
                if let Some(tags) = &tags {
                    git::out(&self.repo, &fetch_args(remote, std::slice::from_ref(tags)))
                        .with_context(|| format!("base: fetching {remote} tags"))?;
                }
            }
            let mut args = vec!["rev-parse"];
            args.extend(fetches.iter().map(|f| f.holding.as_str()));
            let oids = git::out(&self.repo, &args)?;
            for (fetch, oid) in fetches.iter().zip(oids.lines()) {
                let oid = oid.to_string();
                match fetch.want {
                    Want::Base => live.base = Some(oid),
                    Want::Entry(entry) => {
                        live.entries.insert(entry.name.clone(), oid);
                    }
                    Want::Parent(entry, parent) => {
                        live.parents
                            .insert((entry.name.clone(), parent.name.clone()), oid);
                    }
                }
            }
        }
        Ok(live)
    }

    /// What `build` must fetch before resolving pins: whatever has no pin
    /// yet, or is pinned at a commit missing from the object store. An
    /// unpinned patch entry rides along to have its blob hashed.
    pub fn missing_pins<'a>(&'a self, pins: &lock::Pins) -> Vec<Want<'a>> {
        let absent = |pin: Option<&String>| pin.is_none_or(|p| !git::has_commit(&self.repo, p));
        let mut wants = Vec::new();
        if absent(pins.base.as_ref()) {
            wants.push(Want::Base);
        }
        for entry in &self.manifest.entries {
            let pin = pins.entries.get(&entry.name);
            if pin.is_none() || (!entry.kind.is_patch() && absent(pin)) {
                wants.push(Want::Entry(entry));
            }
            let parent_pins = pins.parents.get(&entry.name);
            for parent in &entry.parents {
                if absent(parent_pins.and_then(|p| p.get(&parent.name))) {
                    wants.push(Want::Parent(entry, parent));
                }
            }
        }
        wants
    }

    /// Make sure a pinned commit is in the object store, taking its live ref
    /// from the batch `build` fetched when permitted. `what` names the pin for
    /// the two refusals.
    fn ensure_present(
        &self,
        pin: &str,
        locked: bool,
        what: &str,
        entry: &str,
        fetch: impl FnOnce() -> Result<String>,
    ) -> Result<()> {
        if git::has_commit(&self.repo, pin) {
            return Ok(());
        }
        if locked {
            bail!("{what} is pinned at {pin}, which is not present locally, and --locked forbids fetching");
        }
        fetch()?;
        if !git::has_commit(&self.repo, pin) {
            bail!(
                "{what} is pinned at {pin}, which is not reachable from its live ref; \
             the branch moved on or was rewritten (fetch it manually or \
             `fork-assembler update {entry}`)"
            );
        }
        Ok(())
    }

    /// Resolve the pin for one entry, pinning from live refs when permitted.
    pub fn ensure_pin(
        &self,
        entry: &Entry,
        pins: &mut BTreeMap<String, String>,
        live: &Live,
        locked: bool,
    ) -> Result<String> {
        if let Some(pin) = pins.get(&entry.name) {
            let pin = pin.clone();
            match &entry.kind {
                Kind::Patch { path } => {
                    if patch_blob(&self.root, path)? != pin {
                        bail!(
                        "{}: patch file content changed since it was pinned; run `fork-assembler update {}`",
                        entry.name,
                        entry.name
                    );
                    }
                }
                _ => self
                    .ensure_present(&pin, locked, &entry.name, &entry.name, || live.entry(entry))?,
            }
            return Ok(pin);
        }
        if locked {
            bail!(
            "{}: no pin recorded and --locked forbids pinning; run `fork-assembler build` or `update` first",
            entry.name
        );
        }
        let oid = live.entry(entry)?;
        self.emit(Event::Pinned {
            entry: entry.name.clone(),
            oid: oid.clone(),
        });
        pins.insert(entry.name.clone(), oid.clone());
        Ok(oid)
    }

    /// Resolve every parent pin of one derived entry, pinning from live refs on
    /// its first build. Parents obey the entry pin rule exactly: `build` may
    /// establish one that has never been pinned, and moves none that has.
    pub fn ensure_parent_pins(
        &self,
        entry: &Entry,
        parent_pins: &mut BTreeMap<String, BTreeMap<String, String>>,
        live: &Live,
        locked: bool,
    ) -> Result<()> {
        let mut pins = parent_pins.get(&entry.name).cloned().unwrap_or_default();
        for parent in &entry.parents {
            let what = format!("{}: parent {}", entry.name, parent.name);
            match pins.get(&parent.name).cloned() {
                Some(pin) => self.ensure_present(&pin, locked, &what, &entry.name, || {
                    live.parent(entry, parent)
                })?,
                None => {
                    if locked {
                        bail!(
                            "{what} has no pin recorded and --locked forbids pinning; \
                         run `fork-assembler build` or `update {}` first",
                            entry.name
                        );
                    }
                    let oid = live.parent(entry, parent)?;
                    self.emit(Event::PinnedParent {
                        entry: entry.name.clone(),
                        parent: parent.name.clone(),
                        oid: oid.clone(),
                    });
                    pins.insert(parent.name.clone(), oid);
                }
            }
        }
        // Parents that the manifest no longer declares stop being facts about this
        // entry the moment the declaration goes.
        pins.retain(|name, _| entry.parents.iter().any(|p| &p.name == name));
        parent_pins.insert(entry.name.clone(), pins);
        Ok(())
    }
}
