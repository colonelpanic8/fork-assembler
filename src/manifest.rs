//! `manifest.toml` — INTENT: named remotes, the base, the ordered entries,
//! and the targets deliberately not carried.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use toml_edit::{DocumentMut, Item};

pub const FILE: &str = "manifest.toml";

#[derive(Deserialize)]
struct RawManifest {
    #[serde(default)]
    remotes: BTreeMap<String, String>,
    base: RawBase,
    provenance_file: Option<String>,
    publish: Option<Publish>,
    #[serde(default, rename = "entry")]
    entries: Vec<RawEntry>,
    #[serde(default, rename = "exclude")]
    excludes: Vec<RawExclude>,
}

#[derive(Deserialize)]
struct RawBase {
    remote: String,
    #[serde(rename = "ref")]
    ref_: String,
    submodule: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct Publish {
    pub remote: Option<String>,
    pub branch: Option<String>,
    #[allow(dead_code)] // provenance/description now; push support may come later
    pub tag_prefix: Option<String>,
}

#[derive(Deserialize)]
struct RawEntry {
    name: Option<String>,
    branch: Option<String>,
    pr: Option<i64>,
    remote: Option<String>,
    patch: Option<String>,
    fixup: Option<String>,
    summary: Option<String>,
    note: Option<String>,
    /// Inline `parents = [{ pr = N }, ...]` and nested `[[entry.parents]]`
    /// tables are the same array to serde, so both spellings land here.
    #[serde(default)]
    parents: Vec<RawParent>,
}

#[derive(Deserialize)]
struct RawParent {
    name: Option<String>,
    branch: Option<String>,
    pr: Option<i64>,
    remote: Option<String>,
    /// Never valid; accepted by the parser only so the rejection can explain
    /// itself instead of arriving as "unknown field".
    patch: Option<String>,
}

#[derive(Deserialize)]
struct RawExclude {
    branch: Option<String>,
    pr: Option<i64>,
    patch: Option<String>,
    reason: Option<String>,
}

pub struct Manifest {
    pub remotes: BTreeMap<String, String>,
    pub base: Base,
    pub provenance_file: Option<String>,
    pub publish: Option<Publish>,
    pub entries: Vec<Entry>,
    pub excludes: Vec<Exclusion>,
}

pub struct Base {
    pub remote: String,
    pub ref_: String,
    pub submodule: Option<String>,
}

#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    /// Coherence fixup: a tracked patch applied as part of THIS entry's step,
    /// immediately after its merge commits. It exists because admitting this
    /// entry alongside the ones before it breaks something no single branch
    /// owns (two topics claiming one migration number, a resolution needing
    /// an edit outside any conflict hunk). Binding it to the entry — rather
    /// than to a standalone patch entry sitting later in the order — keeps
    /// every entry boundary a coherent tree and makes the dependency visible
    /// to `remove`/`prune`.
    pub fixup: Option<String>,
    /// Live refs whose commits this entry already contains, in the order it
    /// merged them. Empty for an ordinary entry; non-empty makes the entry
    /// **derived**, and `build` reconstructs it rather than merging its pin.
    pub parents: Vec<Parent>,
    pub summary: Option<String>,
    pub note: Option<String>,
}

/// A live ref whose commits a derived entry already contains.
///
/// The motivating shape is a combined PR: it merges two other PRs and adds its
/// own work on top. The manifest carries only the combination and keeps the
/// parents out, which leaves the most interesting fact about it unrecorded —
/// when a parent moves, nothing knows the combination is stale, and nothing
/// knows how to rebuild it. Declaring the parents states the relationship, and
/// a stated relationship is one `build` can act on: re-merge the parents onto
/// the pinned base, replay the entry's own commits on top, and merge that
/// reconstruction into the stack.
///
/// A parent takes the shapes an entry takes minus `patch`. A patch has no
/// commits to re-merge, so it can be nobody's parent.
#[derive(Clone)]
pub struct Parent {
    pub name: String,
    pub kind: Kind,
}

impl Parent {
    /// Stable human identity of what this parent tracks, for lock snapshots —
    /// the same spelling `Entry::source` uses, since they name the same kinds
    /// of thing.
    pub fn source(&self) -> String {
        match &self.kind {
            Kind::Branch { remote, branch, .. } => format!("{remote}:{branch}"),
            Kind::Pr { remote, number } => format!("{remote}#{number}"),
            Kind::Patch { path } => path.clone(),
        }
    }

    pub fn pr_number(&self) -> Option<i64> {
        match &self.kind {
            Kind::Branch { pr, .. } => *pr,
            Kind::Pr { number, .. } => Some(*number),
            Kind::Patch { .. } => None,
        }
    }

    /// The target this parent names, so refusals can talk about it in `add`'s
    /// vocabulary.
    pub fn target(&self) -> Target {
        match &self.kind {
            Kind::Pr { number, .. } => Target::Pr { number: *number },
            _ => Target::Branch {
                spec: self.source(),
            },
        }
    }
}

#[derive(Clone)]
pub enum Kind {
    /// A live topic branch on a named remote. `pr` is optional metadata: the
    /// PR this branch is published as (provenance link + `add --prs-from`
    /// dedup); it does not change merge behavior.
    Branch {
        remote: String,
        branch: String,
        pr: Option<i64>,
    },
    /// refs/pull/N/head on a named remote (default: the base remote).
    Pr { remote: String, number: i64 },
    /// A tracked patch file applied on top.
    Patch { path: String },
}

impl Kind {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Kind::Branch { .. } => "branch",
            Kind::Pr { .. } => "pr",
            Kind::Patch { .. } => "patch",
        }
    }
}

impl Entry {
    /// Stable human identity of what this entry tracks, for lock snapshots.
    pub fn source(&self) -> String {
        match &self.kind {
            Kind::Branch { remote, branch, .. } => format!("{remote}:{branch}"),
            Kind::Pr { remote, number } => format!("{remote}#{number}"),
            Kind::Patch { path } => path.clone(),
        }
    }

    pub fn pr_number(&self) -> Option<i64> {
        match &self.kind {
            Kind::Branch { pr, .. } => *pr,
            Kind::Pr { number, .. } => Some(*number),
            Kind::Patch { .. } => None,
        }
    }

    /// Does this entry track the same live ref `parent` names? The same
    /// question `Exclusion::matches` asks, asked of a parent declaration:
    /// a PR number wherever each side surfaces it, or the same branch spec.
    pub fn tracks_parent(&self, parent: &Parent) -> bool {
        if let (Some(mine), Some(theirs)) = (self.pr_number(), parent.pr_number()) {
            if mine == theirs {
                return true;
            }
        }
        matches!(self.kind, Kind::Branch { .. })
            && matches!(parent.kind, Kind::Branch { .. })
            && self.source() == parent.source()
    }
}

/// A target deliberately NOT carried.
///
/// Absence from the entry list records nothing. Discovery
/// (`add --prs-from`) appends every open PR it finds, so commenting an entry
/// out is inert — the next sweep puts it back, and carrying a PR alongside a
/// combined branch that already contains it duplicates its commits. An
/// exclusion is the positive statement that a target must stay out: it is the
/// only thing about a non-carried target that survives a sweep, so it is the
/// only correct place to say no.
///
/// `reason` is not load-bearing; recording one is the point of making the
/// refusal declarative, since an exclusion nobody can justify six months
/// later is indistinguishable from an oversight. It is quoted everywhere an
/// exclusion is reported.
#[derive(Clone)]
pub struct Exclusion {
    pub target: Target,
    pub reason: Option<String>,
}

/// What an exclusion names: the same three shapes an entry can take, minus
/// everything that only matters to a merge. An exclusion has no step, so it
/// has no remote to fetch from, no position, and no fixup.
#[derive(Clone, PartialEq, Eq)]
pub enum Target {
    /// A topic branch, as the `REMOTE:BRANCH` spec an entry would name.
    Branch { spec: String },
    /// A PR number, matched wherever it surfaces: a `pr` entry, a branch
    /// entry carrying `pr = N` metadata, or a PR found by discovery.
    Pr { number: i64 },
    /// A tracked patch file.
    Patch { path: String },
}

impl Target {
    /// How the CLI names this target, in `add`'s vocabulary.
    pub fn label(&self) -> String {
        match self {
            Target::Branch { spec } => format!("branch {spec}"),
            Target::Pr { number } => format!("pr {number}"),
            Target::Patch { path } => format!("patch {path}"),
        }
    }
}

impl Exclusion {
    /// Build one from the three mutually exclusive target fields. Shared by
    /// `load` and by the verbs that read the live document, so both reject
    /// the same shapes in the same words.
    pub fn from_fields(
        branch: Option<String>,
        pr: Option<i64>,
        patch: Option<String>,
        reason: Option<String>,
    ) -> Result<Exclusion> {
        let target = match (branch, pr, patch) {
            (Some(spec), None, None) => {
                if !spec.contains(':') {
                    bail!("exclusion {spec:?}: branch targets are REMOTE:BRANCH");
                }
                Target::Branch { spec }
            }
            (None, Some(number), None) => Target::Pr { number },
            (None, None, Some(path)) => Target::Patch { path },
            _ => bail!(
                "an exclusion must name exactly one of: `branch = \"remote:branch\"`, \
                 `pr = N`, or `patch = \"file\"`"
            ),
        };
        Ok(Exclusion { target, reason })
    }

    /// Does `entry` carry what this exclusion refuses?
    pub fn matches(&self, entry: &Entry) -> bool {
        match (&self.target, &entry.kind) {
            (Target::Pr { number }, _) => entry.pr_number() == Some(*number),
            (Target::Branch { spec }, Kind::Branch { .. }) => &entry.source() == spec,
            (Target::Patch { path }, Kind::Patch { path: carried }) => carried == path,
            _ => false,
        }
    }

    /// Does `parent` name what this exclusion refuses? Matched exactly as an
    /// entry is: a parent has the same identity a carried target has, and the
    /// collision it creates is the same kind of contradiction.
    pub fn matches_parent(&self, parent: &Parent) -> bool {
        match (&self.target, &parent.kind) {
            (Target::Pr { number }, _) => parent.pr_number() == Some(*number),
            (Target::Branch { spec }, Kind::Branch { .. }) => &parent.source() == spec,
            _ => false,
        }
    }

    /// Label plus the recorded reason: what every message reporting this
    /// exclusion prints, so the refusal always arrives with its justification.
    pub fn describe(&self) -> String {
        match &self.reason {
            Some(reason) => format!("{} ({reason})", self.target.label()),
            None => format!("{} (no reason recorded)", self.target.label()),
        }
    }
}

impl Manifest {
    pub fn remote_url(&self, name: &str) -> Result<&str> {
        self.remotes
            .get(name)
            .map(String::as_str)
            .with_context(|| format!("remote {name:?} is not defined under [remotes]"))
    }

    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }
}

/// Keep entry-derived names path-safe for refs and generated labels.
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c == '/' || c.is_whitespace() {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// The name an entry answers to: its explicit `name`, else one derived from
/// what it tracks. One rule, shared by the typed load path and by the verbs
/// that read `manifest.toml` as a document, so a name never depends on which
/// reader asked.
pub fn entry_name(
    name: Option<&str>,
    branch: Option<&str>,
    pr: Option<i64>,
    patch: Option<&str>,
) -> Result<String> {
    if let Some(name) = name {
        return Ok(name.to_string());
    }
    if let Some(spec) = branch {
        let branch = spec.split_once(':').map_or(spec, |(_, b)| b);
        return Ok(branch.to_string());
    }
    if let Some(number) = pr {
        return Ok(format!("pr-{number}"));
    }
    if let Some(path) = patch {
        return Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .with_context(|| format!("cannot derive a name from patch path {path:?}"));
    }
    bail!("an entry must name a `branch`, a `pr`, or a `patch`");
}

/// One `parents` element: the entry shapes minus `patch`, named by the same
/// rules an entry is named by, so a parent and the entry it could have been
/// answer to the same name.
fn convert_parent(raw: RawParent, base_remote: &str, entry: &str) -> Result<Parent> {
    if let Some(path) = &raw.patch {
        bail!(
            "entry {entry:?}: parent {path:?} is a patch; a parent is a live ref whose \
             commits this entry merged in, and a patch has no commits to re-merge"
        );
    }
    let kind = match (&raw.branch, raw.pr) {
        (Some(spec), pr) => {
            if raw.remote.is_some() {
                bail!(
                    "entry {entry:?}: parent {spec:?} names its remote as REMOTE:BRANCH; \
                     drop the `remote` field"
                );
            }
            let Some((remote, branch)) = spec.split_once(':') else {
                bail!("entry {entry:?}: parent {spec:?}: branch parents are REMOTE:BRANCH");
            };
            Kind::Branch {
                remote: remote.to_string(),
                branch: branch.to_string(),
                pr,
            }
        }
        (None, Some(number)) => Kind::Pr {
            remote: raw
                .remote
                .clone()
                .unwrap_or_else(|| base_remote.to_string()),
            number,
        },
        (None, None) => bail!(
            "entry {entry:?}: a parent must name a `branch = \"remote:branch\"` or a `pr = N`"
        ),
    };
    let name = entry_name(raw.name.as_deref(), raw.branch.as_deref(), raw.pr, None)?;
    if name.is_empty() {
        bail!("entry {entry:?}: a parent's name cannot be empty");
    }
    Ok(Parent { name, kind })
}

fn convert_entry(raw: RawEntry, base_remote: &str) -> Result<Entry> {
    let kind = match (&raw.branch, raw.pr, &raw.patch) {
        (Some(spec), pr, None) => {
            if raw.remote.is_some() {
                bail!(
                    "entry {spec:?}: branch entries name their remote as REMOTE:BRANCH; \
                     drop the `remote` field"
                );
            }
            let Some((remote, branch)) = spec.split_once(':') else {
                bail!("entry {spec:?}: branch entries are REMOTE:BRANCH");
            };
            Kind::Branch {
                remote: remote.to_string(),
                branch: branch.to_string(),
                pr,
            }
        }
        (None, Some(number), None) => Kind::Pr {
            remote: raw
                .remote
                .clone()
                .unwrap_or_else(|| base_remote.to_string()),
            number,
        },
        (None, None, Some(path)) => Kind::Patch { path: path.clone() },
        _ => bail!(
            "an entry must be exactly one of: `branch = \"remote:branch\"` \
             (optionally with `pr = N` metadata), `pr = N`, or `patch = \"file\"`"
        ),
    };
    let name = entry_name(
        raw.name.as_deref(),
        raw.branch.as_deref(),
        raw.pr,
        raw.patch.as_deref(),
    )?;
    if name.is_empty() || name == "base" {
        bail!("invalid entry name {name:?} (empty and \"base\" are reserved)");
    }
    if raw.fixup.is_some() && matches!(kind, Kind::Patch { .. }) {
        bail!(
            "entry {name:?}: patch entries cannot carry a `fixup` \
             (a patch that needs fixing up is just a patch that needs editing)"
        );
    }
    if !raw.parents.is_empty() && matches!(kind, Kind::Patch { .. }) {
        bail!(
            "entry {name:?}: patch entries cannot carry `parents` \
             (parents describe commits a branch merged in, and a patch has no history \
             to reconstruct)"
        );
    }
    let mut parents = Vec::new();
    let mut named = BTreeSet::new();
    for raw_parent in raw.parents {
        let parent = convert_parent(raw_parent, base_remote, &name)?;
        if !named.insert(parent.name.clone()) {
            bail!(
                "entry {name:?}: duplicate parent name {:?}; set an explicit `name` on one of them",
                parent.name
            );
        }
        parents.push(parent);
    }
    Ok(Entry {
        name,
        kind,
        fixup: raw.fixup,
        parents,
        summary: raw.summary,
        note: raw.note,
    })
}

pub fn load(root: &Path) -> Result<Manifest> {
    let path = root.join(FILE);
    if !path.exists() {
        bail!(
            "no {FILE} in {} (run from the maintenance repo root)",
            root.display()
        );
    }
    let raw: RawManifest = toml::from_str(&fs::read_to_string(&path)?)
        .with_context(|| format!("parsing {}", path.display()))?;

    let entries = raw
        .entries
        .into_iter()
        .map(|e| convert_entry(e, &raw.base.remote))
        .collect::<Result<Vec<_>>>()?;

    let mut seen = BTreeSet::new();
    for entry in &entries {
        if !seen.insert(entry.name.clone()) {
            bail!(
                "duplicate entry name {:?}; set an explicit `name` on one of them",
                entry.name
            );
        }
    }

    let excludes = raw
        .excludes
        .into_iter()
        .map(|x| Exclusion::from_fields(x.branch, x.pr, x.patch, x.reason))
        .collect::<Result<Vec<_>>>()?;

    // Carrying and refusing the same target is not a precedence question with
    // a defensible answer: one of the two statements is a mistake, and only
    // the maintainer knows which. Fail every command until it is resolved,
    // rather than silently honoring whichever the code happens to read last.
    for exclusion in &excludes {
        if let Some(entry) = entries.iter().find(|e| exclusion.matches(e)) {
            bail!(
                "manifest both carries and excludes {}: entry {:?} tracks it \
                 while an [[exclude]] refuses it -- delete whichever is wrong",
                exclusion.describe(),
                entry.name
            );
        }
    }

    // A parent is already carried -- by the entry that merged it. Carrying it
    // again as an entry of its own merges the same commits twice, which is the
    // exact duplication the parent declaration exists to describe. Two entries
    // may share a parent (each reconstruction is standalone, and identical
    // content merges cleanly); an entry and a parent may not name one target.
    for entry in &entries {
        for parent in &entry.parents {
            if let Some(carried) = entries.iter().find(|e| e.tracks_parent(parent)) {
                bail!(
                    "{} is carried both as entry {:?} and as a parent of {:?} -- drop one; \
                     a derived entry already contains its parents' commits, so carrying a \
                     parent alongside it merges them twice",
                    parent.target().label(),
                    carried.name,
                    entry.name
                );
            }
            if let Some(exclusion) = excludes.iter().find(|x| x.matches_parent(parent)) {
                bail!(
                    "{} is declared as a parent of {:?} and also refused by an [[exclude]] \
                     ({}) -- delete the exclusion; declaring a parent already keeps discovery \
                     away from it, and states why",
                    parent.target().label(),
                    entry.name,
                    exclusion
                        .reason
                        .clone()
                        .unwrap_or_else(|| "no reason recorded".into()),
                );
            }
        }
    }

    let manifest = Manifest {
        remotes: raw.remotes,
        base: Base {
            remote: raw.base.remote,
            ref_: raw.base.ref_,
            submodule: raw.base.submodule,
        },
        provenance_file: raw.provenance_file,
        publish: raw.publish,
        entries,
        excludes,
    };

    manifest.remote_url(&manifest.base.remote)?;
    for entry in &manifest.entries {
        for kind in std::iter::once(&entry.kind).chain(entry.parents.iter().map(|p| &p.kind)) {
            match kind {
                Kind::Branch { remote, .. } | Kind::Pr { remote, .. } => {
                    manifest.remote_url(remote)?;
                }
                Kind::Patch { .. } => {}
            }
        }
    }
    Ok(manifest)
}

/// owner/repo slug from a GitHub-style remote URL, for gh.
pub fn slug_from_url(url: &str) -> Result<String> {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let tail = trimmed.rsplit(':').next().unwrap_or(trimmed);
    let mut parts = tail.rsplit('/');
    let repo = parts.next();
    let owner = parts.next();
    match (owner, repo) {
        (Some(o), Some(r)) if !o.is_empty() && !r.is_empty() => Ok(format!("{o}/{r}")),
        _ => bail!("cannot derive owner/repo from remote url {url:?}"),
    }
}

/// What removing entries detached along with them.
pub struct Removal {
    /// Position of the earliest removed entry: the suffix invalidated.
    pub earliest: usize,
    /// (entry name, fixup path) for each removed entry that carried a
    /// coherence fixup. The files are left on disk: a fixup is owned by the
    /// *interaction* between entries, so when one side of that interaction
    /// leaves (typically because it landed upstream), the incoherence it
    /// repaired often persists and the patch needs re-homing rather than
    /// deleting. Callers must surface these.
    pub orphaned_fixups: Vec<(String, String)>,
    /// (entry name, parent labels) for each removed derived entry. A parent
    /// was kept out of the entry list by the declaration that just vanished,
    /// so unlike a fixup it leaves no file behind — it leaves a hole in the
    /// manifest's intent, and an open PR discovery will offer again. Callers
    /// must surface these.
    pub orphaned_parents: Vec<(String, Vec<String>)>,
}

/// Remove the named entries from manifest.toml (comment-preserving).
pub fn remove_entries(root: &Path, names: &[String]) -> Result<Removal> {
    let manifest = load(root)?;
    let all = manifest.names();
    let mut indices = Vec::new();
    for name in names {
        let idx = all
            .iter()
            .position(|n| n == name)
            .with_context(|| format!("no entry named {name:?} in the manifest"))?;
        indices.push(idx);
    }
    indices.sort_unstable();
    indices.dedup();
    let earliest = indices[0];
    let orphaned_fixups = indices
        .iter()
        .filter_map(|idx| {
            let entry = &manifest.entries[*idx];
            entry.fixup.clone().map(|path| (entry.name.clone(), path))
        })
        .collect();
    let orphaned_parents = indices
        .iter()
        .filter_map(|idx| {
            let entry = &manifest.entries[*idx];
            let labels: Vec<String> = entry.parents.iter().map(|p| p.target().label()).collect();
            (!labels.is_empty()).then(|| (entry.name.clone(), labels))
        })
        .collect();

    let path = root.join(FILE);
    let mut doc = fs::read_to_string(&path)?
        .parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))?;
    let arr = doc
        .get_mut("entry")
        .and_then(Item::as_array_of_tables_mut)
        .context("manifest has no [[entry]] tables")?;
    for idx in indices.iter().rev() {
        arr.remove(*idx);
    }
    fs::write(&path, doc.to_string())?;
    Ok(Removal {
        earliest,
        orphaned_fixups,
        orphaned_parents,
    })
}

/// What `record_exclusion` did to the manifest.
pub enum Excluded {
    /// A new `[[exclude]]` table was appended.
    Added,
    /// The target was already excluded, with the reason left as it was.
    AlreadyRecorded,
    /// The target was already excluded and its reason was replaced.
    ReasonUpdated { previous: Option<String> },
}

/// Record an exclusion in manifest.toml (comment-preserving), idempotently.
///
/// Refuses when the target is currently carried. Removing an entry
/// invalidates every later entry's build, and that consequence belongs to
/// `remove`, which reports it; a bookkeeping verb must not trigger it as a
/// side effect.
///
/// Never touches the lock: an exclusion says nothing about any assembled
/// tree, so nothing needs rebuilding after one.
pub fn record_exclusion(root: &Path, target: &Target, reason: Option<&str>) -> Result<Excluded> {
    let manifest = load(root)?;
    let exclusion = Exclusion {
        target: target.clone(),
        reason: reason.map(str::to_string),
    };
    if let Some(entry) = manifest.entries.iter().find(|e| exclusion.matches(e)) {
        bail!(
            "{} is carried by entry {:?}; run `fork-fold remove {}` first \
             (that invalidates the build from its position, which is why \
             excluding will not do it for you)",
            target.label(),
            entry.name,
            entry.name
        );
    }

    let path = root.join(FILE);
    let mut doc = fs::read_to_string(&path)?
        .parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))?;
    let existing = doc
        .get("exclude")
        .and_then(Item::as_array_of_tables)
        .map(|arr| {
            arr.iter()
                .position(|t| read_exclude_target(t).as_ref() == Some(target))
        })
        .unwrap_or(None);

    if let Some(index) = existing {
        let table = doc
            .get_mut("exclude")
            .and_then(Item::as_array_of_tables_mut)
            .and_then(|arr| arr.get_mut(index))
            .context("exclusion vanished between read and edit")?;
        let previous = table
            .get("reason")
            .and_then(Item::as_str)
            .map(str::to_string);
        match reason {
            Some(reason) if previous.as_deref() != Some(reason) => {
                table["reason"] = toml_edit::value(reason);
                fs::write(&path, doc.to_string())?;
                return Ok(Excluded::ReasonUpdated { previous });
            }
            _ => return Ok(Excluded::AlreadyRecorded),
        }
    }

    let excludes = doc
        .entry("exclude")
        .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    let arr = excludes
        .as_array_of_tables_mut()
        .context("`exclude` is set to something other than [[exclude]] tables")?;
    let mut table = toml_edit::Table::new();
    match target {
        Target::Branch { spec } => table["branch"] = toml_edit::value(spec.as_str()),
        Target::Pr { number } => table["pr"] = toml_edit::value(*number),
        Target::Patch { path } => table["patch"] = toml_edit::value(path.as_str()),
    }
    if let Some(reason) = reason {
        table["reason"] = toml_edit::value(reason);
    }
    arr.push(table);
    fs::write(&path, doc.to_string())?;
    Ok(Excluded::Added)
}

/// The target an `[[exclude]]` table names, reading the live document rather
/// than the typed load, so the editing verbs see exactly what is on disk.
pub fn read_exclude_target(table: &toml_edit::Table) -> Option<Target> {
    if let Some(spec) = table.get("branch").and_then(Item::as_str) {
        return Some(Target::Branch {
            spec: spec.to_string(),
        });
    }
    if let Some(number) = table.get("pr").and_then(Item::as_integer) {
        return Some(Target::Pr { number });
    }
    table
        .get("patch")
        .and_then(Item::as_str)
        .map(|path| Target::Patch {
            path: path.to_string(),
        })
}

/// Attach (`Some`) or detach (`None`) an entry's coherence fixup in
/// manifest.toml. Returns the entry's position: the suffix a rebuild must
/// redo, since the entry's own step now produces a different tree.
pub fn set_fixup(root: &Path, name: &str, fixup: Option<&str>) -> Result<usize> {
    let manifest = load(root)?;
    let index = manifest
        .entries
        .iter()
        .position(|e| e.name == name)
        .with_context(|| format!("no entry named {name:?} in the manifest"))?;
    if matches!(manifest.entries[index].kind, Kind::Patch { .. }) {
        bail!(
            "{name}: patch entries cannot carry a fixup; edit {} instead",
            manifest.entries[index].source()
        );
    }

    let path = root.join(FILE);
    let mut doc = fs::read_to_string(&path)?
        .parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))?;
    let table = doc
        .get_mut("entry")
        .and_then(Item::as_array_of_tables_mut)
        .context("manifest has no [[entry]] tables")?
        .get_mut(index)
        .context("entry vanished between load and edit")?;
    match fixup {
        Some(rel) => table["fixup"] = toml_edit::value(rel),
        None => {
            table.remove("fixup");
        }
    }
    fs::write(&path, doc.to_string())?;
    Ok(index)
}
