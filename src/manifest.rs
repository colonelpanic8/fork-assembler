//! `manifest.toml` — INTENT: named remotes, the base, the ordered entries,
//! and the targets deliberately not carried.
//!
//! This is the typed view: `load` parses and validates the whole document.
//! The verbs that edit the file in place, comment-preserving, live in `edit`.

pub mod edit;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

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
    reconstruction_publish: Option<String>,
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

#[derive(Clone)]
pub struct Manifest {
    pub remotes: BTreeMap<String, String>,
    pub base: Base,
    pub provenance_file: Option<String>,
    pub publish: Option<Publish>,
    pub entries: Vec<Entry>,
    pub excludes: Vec<Exclusion>,
}

#[derive(Clone)]
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
    /// Optional writable branch that receives a completed reconstruction.
    /// PR entries need this because `refs/pull/N/head` does not identify a
    /// writable remote branch.
    pub reconstruction_publish: Option<ReconstructionPublish>,
    /// Live refs whose commits this entry already contains, in the order it
    /// merged them. Empty for an ordinary entry; non-empty makes the entry
    /// **derived**, and `build` reconstructs it rather than merging its pin.
    pub parents: Vec<Parent>,
    pub summary: Option<String>,
    pub note: Option<String>,
}

/// A branch to update after a derived entry has been reconstructed.
#[derive(Clone)]
pub struct ReconstructionPublish {
    pub remote: String,
    pub branch: String,
}

impl ReconstructionPublish {
    pub fn source(&self) -> String {
        format!("{}:{}", self.remote, self.branch)
    }
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

    pub fn is_patch(&self) -> bool {
        matches!(self, Kind::Patch { .. })
    }

    /// Stable human identity of what this tracks, as `status` and the lock
    /// spell it.
    pub fn source(&self) -> String {
        match self {
            Kind::Branch { remote, branch, .. } => format!("{remote}:{branch}"),
            Kind::Pr { remote, number } => format!("{remote}#{number}"),
            Kind::Patch { path } => path.clone(),
        }
    }

    pub fn pr_number(&self) -> Option<i64> {
        match self {
            Kind::Branch { pr, .. } => *pr,
            Kind::Pr { number, .. } => Some(*number),
            Kind::Patch { .. } => None,
        }
    }

    /// The remote and the ref on it to fetch, for the two kinds that track
    /// one.
    pub fn remote_ref(&self) -> Option<(&str, String)> {
        match self {
            Kind::Branch { remote, branch, .. } => Some((remote, format!("refs/heads/{branch}"))),
            Kind::Pr { remote, number } => Some((remote, format!("refs/pull/{number}/head"))),
            Kind::Patch { .. } => None,
        }
    }

    /// What this tracks, in `add`'s vocabulary.
    pub fn target(&self) -> Target {
        match self {
            Kind::Branch { .. } => Target::Branch {
                spec: self.source(),
            },
            Kind::Pr { number, .. } => Target::Pr { number: *number },
            Kind::Patch { path } => Target::Patch { path: path.clone() },
        }
    }
}

impl Entry {
    pub fn source(&self) -> String {
        self.kind.source()
    }

    pub fn pr_number(&self) -> Option<i64> {
        self.kind.pr_number()
    }

    pub fn is_derived(&self) -> bool {
        !self.parents.is_empty()
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

impl Parent {
    pub fn source(&self) -> String {
        self.kind.source()
    }

    pub fn pr_number(&self) -> Option<i64> {
        self.kind.pr_number()
    }

    pub fn target(&self) -> Target {
        self.kind.target()
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
#[derive(Clone, PartialEq, Eq, Hash)]
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

    /// Does `kind` — an entry's or a parent's — track what this exclusion
    /// refuses? A PR number matches wherever it surfaces; a branch or patch
    /// matches its own shape exactly.
    pub fn matches(&self, kind: &Kind) -> bool {
        match (&self.target, kind) {
            (Target::Pr { number }, _) => kind.pr_number() == Some(*number),
            (Target::Branch { spec }, Kind::Branch { .. }) => &kind.source() == spec,
            (Target::Patch { path }, Kind::Patch { path: carried }) => carried == path,
            _ => false,
        }
    }

    /// The recorded reason, or the phrase every report uses in its absence.
    pub fn reason_text(&self) -> &str {
        self.reason.as_deref().unwrap_or("no reason recorded")
    }

    /// Label plus the recorded reason: what every message reporting this
    /// exclusion prints, so the refusal always arrives with its justification.
    pub fn describe(&self) -> String {
        format!("{} ({})", self.target.label(), self.reason_text())
    }
}

impl Manifest {
    pub fn remote_url(&self, name: &str) -> Result<&str> {
        self.remotes
            .get(name)
            .map(String::as_str)
            .with_context(|| format!("remote {name:?} is not defined under [remotes]"))
    }

    pub fn has_entry(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    pub fn position(&self, name: &str) -> Result<usize> {
        self.entries
            .iter()
            .position(|e| e.name == name)
            .with_context(|| format!("no entry named {name:?} in the manifest"))
    }
}

/// "entry" or "entries", for counts in reports.
pub fn entries_noun(n: usize) -> &'static str {
    if n == 1 {
        "entry"
    } else {
        "entries"
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

/// The live-ref shapes an entry and a parent share: `branch = "REMOTE:BRANCH"`
/// (optionally with `pr = N` metadata), or `pr = N` on `remote` (default: the
/// base remote). `None` when the declaration names neither.
fn parse_ref_kind(
    subject: &str,
    branch: Option<&str>,
    pr: Option<i64>,
    remote: Option<&str>,
    base_remote: &str,
) -> Result<Option<Kind>> {
    let kind = match (branch, pr) {
        (Some(spec), pr) => {
            if remote.is_some() {
                bail!(
                    "{subject}: branch {spec:?} names its remote as REMOTE:BRANCH; \
                     drop the `remote` field"
                );
            }
            let Some((remote, branch)) = spec.split_once(':') else {
                bail!("{subject}: branch {spec:?} must be REMOTE:BRANCH");
            };
            Kind::Branch {
                remote: remote.to_string(),
                branch: branch.to_string(),
                pr,
            }
        }
        (None, Some(number)) => Kind::Pr {
            remote: remote.unwrap_or(base_remote).to_string(),
            number,
        },
        (None, None) => return Ok(None),
    };
    Ok(Some(kind))
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
    if raw.branch.is_none() && raw.pr.is_none() {
        bail!("entry {entry:?}: a parent must name a `branch = \"remote:branch\"` or a `pr = N`");
    }
    let name = entry_name(raw.name.as_deref(), raw.branch.as_deref(), raw.pr, None)?;
    if name.is_empty() {
        bail!("entry {entry:?}: a parent's name cannot be empty");
    }
    let subject = format!("entry {entry:?}: parent {name:?}");
    let kind = parse_ref_kind(
        &subject,
        raw.branch.as_deref(),
        raw.pr,
        raw.remote.as_deref(),
        base_remote,
    )?
    .expect("a branch or pr is present");
    Ok(Parent { name, kind })
}

fn convert_entry(raw: RawEntry, base_remote: &str) -> Result<Entry> {
    let name = entry_name(
        raw.name.as_deref(),
        raw.branch.as_deref(),
        raw.pr,
        raw.patch.as_deref(),
    )?;
    if name.is_empty() || name == "base" {
        bail!("invalid entry name {name:?} (empty and \"base\" are reserved)");
    }
    let subject = format!("entry {name:?}");
    let live = parse_ref_kind(
        &subject,
        raw.branch.as_deref(),
        raw.pr,
        raw.remote.as_deref(),
        base_remote,
    )?;
    let kind = match (live, raw.patch) {
        (Some(kind), None) => kind,
        (None, Some(path)) => Kind::Patch { path },
        _ => bail!(
            "{subject}: an entry must be exactly one of: `branch = \"remote:branch\"` \
             (optionally with `pr = N` metadata), `pr = N`, or `patch = \"file\"`"
        ),
    };
    if kind.is_patch() {
        if raw.fixup.is_some() {
            bail!(
                "{subject}: patch entries cannot carry a `fixup` \
                 (a patch that needs fixing up is just a patch that needs editing)"
            );
        }
        if !raw.parents.is_empty() {
            bail!(
                "{subject}: patch entries cannot carry `parents` \
                 (parents describe commits a branch merged in, and a patch has no history \
                 to reconstruct)"
            );
        }
    }
    let reconstruction_publish = match raw.reconstruction_publish {
        Some(spec) => {
            if raw.parents.is_empty() {
                bail!("{subject}: `reconstruction_publish` only applies to derived entries with `parents`");
            }
            match spec.split_once(':') {
                Some((remote, branch)) if !remote.is_empty() && !branch.is_empty() => {
                    Some(ReconstructionPublish {
                        remote: remote.to_string(),
                        branch: branch.to_string(),
                    })
                }
                _ => {
                    bail!("{subject}: reconstruction publish target {spec:?} must be REMOTE:BRANCH")
                }
            }
        }
        None => None,
    };
    let mut parents = Vec::new();
    let mut named = BTreeSet::new();
    for raw_parent in raw.parents {
        let parent = convert_parent(raw_parent, base_remote, &name)?;
        if !named.insert(parent.name.clone()) {
            bail!(
                "{subject}: duplicate parent name {:?}; set an explicit `name` on one of them",
                parent.name
            );
        }
        parents.push(parent);
    }
    Ok(Entry {
        name,
        kind,
        fixup: raw.fixup,
        reconstruction_publish,
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
        if let Some(entry) = entries.iter().find(|e| exclusion.matches(&e.kind)) {
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
            if let Some(exclusion) = excludes.iter().find(|x| x.matches(&parent.kind)) {
                bail!(
                    "{} is declared as a parent of {:?} and also refused by an [[exclude]] \
                     ({}) -- delete the exclusion; declaring a parent already keeps discovery \
                     away from it, and states why",
                    parent.target().label(),
                    entry.name,
                    exclusion.reason_text(),
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
            if let Some((remote, _)) = kind.remote_ref() {
                manifest.remote_url(remote)?;
            }
        }
        if let Some(target) = &entry.reconstruction_publish {
            manifest.remote_url(&target.remote)?;
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
