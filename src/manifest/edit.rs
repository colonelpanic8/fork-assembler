//! `manifest.toml` as a live document: the verbs that change it in place.
//!
//! Everything here goes through `toml_edit` so comments and formatting
//! survive, and reads exactly what is on disk rather than the typed load —
//! the editing verbs must see the file as the maintainer wrote it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use super::{entry_name, load, slug_from_url, Exclusion, Kind, Target, FILE};

/// The manifest file, parsed for editing.
pub struct Document {
    path: PathBuf,
    doc: DocumentMut,
}

impl Document {
    pub fn read(root: &Path) -> Result<Document> {
        let path = root.join(FILE);
        if !path.exists() {
            bail!(
                "no {FILE} in {} (run from the maintenance repo root)",
                root.display()
            );
        }
        let doc = fs::read_to_string(&path)?
            .parse::<DocumentMut>()
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Document { path, doc })
    }

    pub fn save(&self) -> Result<()> {
        fs::write(&self.path, self.doc.to_string())
            .with_context(|| format!("writing {}", self.path.display()))
    }

    fn tables(&self, key: &str) -> impl Iterator<Item = &Table> {
        self.doc
            .get(key)
            .and_then(Item::as_array_of_tables)
            .into_iter()
            .flat_map(|arr| arr.iter())
    }

    fn entries_mut(&mut self) -> Result<&mut ArrayOfTables> {
        self.doc
            .get_mut("entry")
            .and_then(Item::as_array_of_tables_mut)
            .context("manifest has no [[entry]] tables")
    }

    /// Append an `[[entry]]` table filled in by `fill`.
    pub fn push_entry(&mut self, fill: impl FnOnce(&mut Table)) {
        let mut table = Table::new();
        fill(&mut table);
        self.doc
            .entry("entry")
            .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
            .as_array_of_tables_mut()
            .expect("entry is an array of tables")
            .push(table);
    }

    /// The exclusions recorded in the document, in manifest order.
    pub fn exclusions(&self) -> Result<Vec<Exclusion>> {
        self.tables("exclude")
            .map(|t| {
                Exclusion::from_fields(
                    str_field(t, "branch"),
                    t.get("pr").and_then(Item::as_integer),
                    str_field(t, "patch"),
                    str_field(t, "reason"),
                )
            })
            .collect()
    }

    /// Every target the document already carries, as an entry or inside one.
    pub fn carried(&self) -> Carried {
        let mut carried = Carried::default();
        for t in self.tables("entry") {
            let branch = str_field(t, "branch");
            let pr = t.get("pr").and_then(Item::as_integer);
            let patch = str_field(t, "patch");
            carried
                .entries
                .extend(targets(branch.as_deref(), pr, patch.as_deref()));
            let carrier = entry_name(
                str_field(t, "name").as_deref(),
                branch.as_deref(),
                pr,
                patch.as_deref(),
            )
            .unwrap_or_else(|_| "<unnamed>".to_string());
            for (pr, branch) in parent_tables(t) {
                for target in targets(branch.as_deref(), pr, None) {
                    carried.parents.insert(target, carrier.clone());
                }
            }
        }
        carried
    }

    /// owner/repo slug of the base remote, for gh.
    pub fn base_repo_slug(&self) -> Result<String> {
        let base_remote = self
            .doc
            .get("base")
            .and_then(|b| b.get("remote"))
            .and_then(Item::as_str)
            .unwrap_or("upstream");
        let url = self
            .doc
            .get("remotes")
            .and_then(|r| r.get(base_remote))
            .and_then(Item::as_str)
            .with_context(|| format!("remote {base_remote:?} not defined under [remotes]"))?;
        slug_from_url(url)
    }
}

/// What the document already carries, keyed the way `add` names things.
#[derive(Default)]
pub struct Carried {
    /// Targets carried as entries. A branch entry with `pr = N` metadata
    /// counts as both.
    pub entries: HashSet<Target>,
    /// Targets some entry declares as a parent, mapped to that entry's name.
    /// They are carried — by the entry that merged them — so nothing may
    /// carry them again, and the refusal has to be able to say by whom.
    pub parents: HashMap<Target, String>,
}

fn str_field(table: &Table, key: &str) -> Option<String> {
    table.get(key).and_then(Item::as_str).map(str::to_string)
}

/// The targets one declaration surfaces: its branch spec, its PR number, or
/// its patch path, whichever are present.
fn targets(branch: Option<&str>, pr: Option<i64>, patch: Option<&str>) -> Vec<Target> {
    let mut out = Vec::new();
    if let Some(spec) = branch {
        out.push(Target::Branch {
            spec: spec.to_string(),
        });
    }
    if let Some(number) = pr {
        out.push(Target::Pr { number });
    }
    if let Some(path) = patch {
        out.push(Target::Patch {
            path: path.to_string(),
        });
    }
    out
}

/// The parent declarations on one `[[entry]]` table, in either spelling:
/// `parents = [{ pr = N }]` reads as an array of inline tables, nested
/// `[[entry.parents]]` as an array of tables, and the typed load treats them
/// as one thing — so the document readers must too.
fn parent_tables(entry: &Table) -> Vec<(Option<i64>, Option<String>)> {
    let from_inline = entry
        .get("parents")
        .and_then(Item::as_array)
        .into_iter()
        .flat_map(|arr| arr.iter())
        .filter_map(|v| v.as_inline_table())
        .map(|t| {
            (
                t.get("pr").and_then(|v| v.as_integer()),
                t.get("branch").and_then(|v| v.as_str()).map(str::to_string),
            )
        });
    let from_tables = entry
        .get("parents")
        .and_then(Item::as_array_of_tables)
        .into_iter()
        .flat_map(|arr| arr.iter())
        .map(|t| {
            (
                t.get("pr").and_then(Item::as_integer),
                str_field(t, "branch"),
            )
        });
    from_inline.chain(from_tables).collect()
}

/// The target an `[[exclude]]` table names.
fn exclude_target(table: &Table) -> Option<Target> {
    targets(
        str_field(table, "branch").as_deref(),
        table.get("pr").and_then(Item::as_integer),
        str_field(table, "patch").as_deref(),
    )
    .into_iter()
    .next()
}

/// What removing entries detached along with them.
#[derive(Serialize)]
pub struct Removal {
    /// Position of the earliest removed entry: the suffix invalidated.
    pub earliest: usize,
    /// Removed entries that carried a coherence fixup. The files are left
    /// on disk: a fixup is owned by the *interaction* between entries, so
    /// when one side of that interaction leaves (typically because it landed
    /// upstream), the incoherence it repaired often persists and the patch
    /// needs re-homing rather than deleting. Callers must surface these.
    pub orphaned_fixups: Vec<OrphanedFixup>,
    /// Removed derived entries and the parents they declared. A parent was
    /// kept out of the entry list by the declaration that just vanished, so
    /// unlike a fixup it leaves no file behind — it leaves a hole in the
    /// manifest's intent, and an open PR discovery will offer again. Callers
    /// must surface these.
    pub orphaned_parents: Vec<OrphanedParents>,
}

#[derive(Serialize)]
pub struct OrphanedFixup {
    pub entry: String,
    pub path: String,
}

#[derive(Serialize)]
pub struct OrphanedParents {
    pub entry: String,
    /// The parents' target labels.
    pub parents: Vec<String>,
}

/// Remove the named entries from manifest.toml (comment-preserving).
pub fn remove_entries(root: &Path, names: &[String]) -> Result<Removal> {
    let manifest = load(root)?;
    let mut indices = names
        .iter()
        .map(|name| manifest.position(name))
        .collect::<Result<Vec<_>>>()?;
    indices.sort_unstable();
    indices.dedup();
    let earliest = indices[0];
    let removed = indices.iter().map(|idx| &manifest.entries[*idx]);
    let orphaned_fixups = removed
        .clone()
        .filter_map(|entry| {
            entry.fixup.clone().map(|path| OrphanedFixup {
                entry: entry.name.clone(),
                path,
            })
        })
        .collect();
    let orphaned_parents = removed
        .filter(|entry| entry.is_derived())
        .map(|entry| OrphanedParents {
            entry: entry.name.clone(),
            parents: entry.parents.iter().map(|p| p.target().label()).collect(),
        })
        .collect();

    let mut doc = Document::read(root)?;
    let arr = doc.entries_mut()?;
    for idx in indices.iter().rev() {
        arr.remove(*idx);
    }
    doc.save()?;
    Ok(Removal {
        earliest,
        orphaned_fixups,
        orphaned_parents,
    })
}

/// What `record_exclusion` did to the manifest.
#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
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
    if let Some(entry) = manifest.entries.iter().find(|e| exclusion.matches(&e.kind)) {
        bail!(
            "{} is carried by entry {:?}; run `fork-assembler remove {}` first \
             (that invalidates the build from its position, which is why \
             excluding will not do it for you)",
            target.label(),
            entry.name,
            entry.name
        );
    }

    let mut doc = Document::read(root)?;
    let existing = doc
        .tables("exclude")
        .position(|t| exclude_target(t).as_ref() == Some(target));

    if let Some(index) = existing {
        let table = doc
            .doc
            .get_mut("exclude")
            .and_then(Item::as_array_of_tables_mut)
            .and_then(|arr| arr.get_mut(index))
            .context("exclusion vanished between read and edit")?;
        let previous = str_field(table, "reason");
        return match reason {
            Some(reason) if previous.as_deref() != Some(reason) => {
                table["reason"] = value(reason);
                doc.save()?;
                Ok(Excluded::ReasonUpdated { previous })
            }
            _ => Ok(Excluded::AlreadyRecorded),
        };
    }

    let mut table = Table::new();
    match target {
        Target::Branch { spec } => table["branch"] = value(spec.as_str()),
        Target::Pr { number } => table["pr"] = value(*number),
        Target::Patch { path } => table["patch"] = value(path.as_str()),
    }
    if let Some(reason) = reason {
        table["reason"] = value(reason);
    }
    doc.doc
        .entry("exclude")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .context("`exclude` is set to something other than [[exclude]] tables")?
        .push(table);
    doc.save()?;
    Ok(Excluded::Added)
}

/// Attach (`Some`) or detach (`None`) an entry's coherence fixup in
/// manifest.toml. Returns the entry's position: the suffix a rebuild must
/// redo, since the entry's own step now produces a different tree.
pub fn set_fixup(root: &Path, name: &str, fixup: Option<&str>) -> Result<usize> {
    let manifest = load(root)?;
    let index = manifest.position(name)?;
    let entry = &manifest.entries[index];
    if let Kind::Patch { path } = &entry.kind {
        bail!("{name}: patch entries cannot carry a fixup; edit {path} instead");
    }

    let mut doc = Document::read(root)?;
    let table = doc
        .entries_mut()?
        .get_mut(index)
        .context("entry vanished between load and edit")?;
    match fixup {
        Some(rel) => table["fixup"] = value(rel),
        None => {
            table.remove("fixup");
        }
    }
    doc.save().map(|()| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parents = [{ pr = N }]` and nested `[[entry.parents]]` are one thing
    /// to the typed load, so the document reader must find both.
    #[test]
    fn parent_targets_are_read_in_both_spellings() {
        let doc: DocumentMut = "[[entry]]\nbranch = \"mine:combined\"\n\
             parents = [{ pr = 11 }, { branch = \"mine:topic\" }]\n\n\
             [[entry]]\npr = 42\n\n[[entry.parents]]\npr = 12\n"
            .parse()
            .expect("manifest parses");
        let doc = Document {
            path: PathBuf::new(),
            doc,
        };
        let carried = doc.carried();
        let pr = |number| Target::Pr { number };
        assert_eq!(carried.parents.get(&pr(11)), Some(&"combined".to_string()));
        assert_eq!(carried.parents.get(&pr(12)), Some(&"pr-42".to_string()));
        assert_eq!(
            carried.parents.get(&Target::Branch {
                spec: "mine:topic".into()
            }),
            Some(&"combined".to_string())
        );
        assert!(carried.entries.contains(&pr(42)));
    }
}
