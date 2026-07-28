//! `manifest.toml` — INTENT: named remotes, the base, and the ordered entries.

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
    summary: Option<String>,
    note: Option<String>,
}

pub struct Manifest {
    pub remotes: BTreeMap<String, String>,
    pub base: Base,
    pub provenance_file: Option<String>,
    pub publish: Option<Publish>,
    pub entries: Vec<Entry>,
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
    pub summary: Option<String>,
    pub note: Option<String>,
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
    let name = match raw.name {
        Some(name) => name,
        None => match &kind {
            Kind::Branch { branch, .. } => branch.clone(),
            Kind::Pr { number, .. } => format!("pr-{number}"),
            Kind::Patch { path } => Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .with_context(|| format!("cannot derive a name from patch path {path:?}"))?
                .to_string(),
        },
    };
    if name.is_empty() || name == "base" {
        bail!("invalid entry name {name:?} (empty and \"base\" are reserved)");
    }
    Ok(Entry {
        name,
        kind,
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
    };

    manifest.remote_url(&manifest.base.remote)?;
    for entry in &manifest.entries {
        match &entry.kind {
            Kind::Branch { remote, .. } | Kind::Pr { remote, .. } => {
                manifest.remote_url(remote)?;
            }
            Kind::Patch { .. } => {}
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

/// Remove the named entries from manifest.toml (comment-preserving).
/// Returns the position of the earliest removed entry.
pub fn remove_entries(root: &Path, names: &[String]) -> Result<usize> {
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
    Ok(earliest)
}
