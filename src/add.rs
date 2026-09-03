//! `add` and `exclude`: the verbs that admit a target to the manifest, or
//! refuse one. Both read `manifest.toml` as a document so that what they
//! append lands beside what the maintainer wrote, comments intact.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use toml_edit::{value, Array, InlineTable, Item};

use crate::manifest::edit::{self, Carried, Document, Excluded};
use crate::manifest::{Exclusion, Target};

/// The refusal an explicitly named target gets when the manifest already
/// says something about it.
///
/// Discovery sweeps are bulk and impersonal, so `--prs-from` skips such PRs
/// with a note. Naming one on the command line is a decision, and the
/// maintainer deserves to learn it contradicts a recorded one rather than
/// watch the request vanish.
fn reject_if_refused(exclusions: &[Exclusion], carried: &Carried, target: &Target) -> Result<()> {
    if let Some(exclusion) = exclusions.iter().find(|x| &x.target == target) {
        bail!(
            "{} is excluded by the manifest: {}\n\
             delete that [[exclude]] first if the refusal no longer holds",
            target.label(),
            exclusion.reason_text()
        );
    }
    if let Some(entry) = carried.parents.get(target) {
        bail!(
            "{} is carried as a parent of entry {entry:?}: its commits are reconstructed \
             into that entry on every build, so carrying it again would merge them twice\n\
             drop that entry's `parents` declaration first if the relationship no longer holds",
            target.label()
        );
    }
    Ok(())
}

fn open_prs_by(slug: &str, author: &str) -> Result<Vec<(i64, String)>> {
    let out = Command::new("gh")
        .args([
            "pr",
            "list",
            "-R",
            slug,
            "--author",
            author,
            "--state",
            "open",
            "--json",
            "number,title",
            "--limit",
            "500",
        ])
        .output()
        .context("failed to run gh")?;
    if !out.status.success() {
        bail!(
            "gh pr list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let mut prs = Vec::new();
    for item in parsed.as_array().context("unexpected gh output")? {
        let number = item["number"].as_i64().context("pr without number")?;
        let title = item["title"].as_str().unwrap_or("").to_string();
        prs.push((number, title));
    }
    prs.sort();
    Ok(prs)
}

/// What a discovery sweep decides about one PR it found.
#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Discovered {
    Append,
    AlreadyCarried,
    /// Carried inside the named entry, which merged it and builds on it. Not
    /// an exclusion: the entry says why this PR stays out of the list, and
    /// says it more precisely than any reason string could.
    CarriedAsParent {
        entry: String,
    },
    Excluded {
        reason: String,
    },
}

/// Sort discovered PRs into append / already-carried / parent / excluded.
///
/// Split out from the sweep so the decision is testable without `gh`: this is
/// the whole of what `--prs-from` decides, and the rules that keep a target
/// out are the part worth proving.
fn triage_discovered(
    prs: &[(i64, String)],
    carried: &HashSet<Target>,
    parents: &HashMap<Target, String>,
    exclusions: &[Exclusion],
) -> Vec<(i64, String, Discovered)> {
    prs.iter()
        .map(|(number, title)| {
            let target = Target::Pr { number: *number };
            let verdict = if carried.contains(&target) {
                Discovered::AlreadyCarried
            } else if let Some(entry) = parents.get(&target) {
                Discovered::CarriedAsParent {
                    entry: entry.clone(),
                }
            } else if let Some(exclusion) = exclusions.iter().find(|x| x.target == target) {
                Discovered::Excluded {
                    reason: exclusion.reason_text().to_string(),
                }
            } else {
                Discovered::Append
            };
            (*number, title.clone(), verdict)
        })
        .collect()
}

#[derive(Serialize)]
pub struct ExcludeReport {
    pub target: Target,
    /// Whether the operator gave a reason; an exclusion without one is
    /// worth a nudge.
    pub reason_given: bool,
    #[serde(flatten)]
    pub outcome: Excluded,
}

pub fn exclude(
    root: &Path,
    target: Option<String>,
    pr: Option<u64>,
    patch: Option<String>,
    reason: Option<String>,
) -> Result<ExcludeReport> {
    let target = Exclusion::from_fields(target, pr.map(|n| n as i64), patch, None)
        .context("nothing to exclude: pass REMOTE:BRANCH, --pr, or --patch")?
        .target;
    let outcome = edit::record_exclusion(root, &target, reason.as_deref())?;
    Ok(ExcludeReport {
        target,
        reason_given: reason.is_some(),
        outcome,
    })
}

/// One `--parent` argument: a PR number, or a REMOTE:BRANCH.
fn parse_parent(spec: &str) -> Result<Target> {
    if !spec.is_empty() && spec.chars().all(|c| c.is_ascii_digit()) {
        return Ok(Target::Pr {
            number: spec.parse()?,
        });
    }
    if !spec.contains(':') {
        bail!("--parent {spec:?}: parents are a PR number or REMOTE:BRANCH");
    }
    Ok(Target::Branch {
        spec: spec.to_string(),
    })
}

/// Render `parents = [{ pr = 2525 }, { branch = "mine:foo" }]` — the inline
/// form, because a parent list is one fact about one entry and reads better
/// beside it than as a run of nested tables.
fn parents_array(parents: &[Target]) -> Array {
    let mut array = Array::new();
    for target in parents {
        let mut table = InlineTable::new();
        match target {
            Target::Pr { number } => {
                table.insert("pr", (*number).into());
            }
            Target::Branch { spec } => {
                table.insert("branch", spec.as_str().into());
            }
            // `add` never builds a patch target for a parent: parse_parent
            // cannot produce one.
            Target::Patch { path } => {
                table.insert("patch", path.as_str().into());
            }
        }
        array.push(table);
    }
    array
}

/// One thing `add` did, in the order it did them.
#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Admitted {
    Added {
        target: Target,
        parents: Vec<Target>,
    },
    AlreadyCarried {
        target: Target,
    },
    NoOpenPrs {
        author: String,
        slug: String,
    },
    /// One PR a `--prs-from` sweep found, and what was decided about it.
    Discovered {
        number: i64,
        title: String,
        #[serde(flatten)]
        verdict: Discovered,
    },
}

#[derive(Serialize)]
pub struct AddReport {
    pub log: Vec<Admitted>,
    /// Entries appended to the manifest; zero means it was left untouched.
    pub appended: usize,
}

/// One explicitly named target: refused, already carried, or appended with
/// its parents.
fn admit(
    doc: &mut Document,
    carried: &Carried,
    exclusions: &[Exclusion],
    target: Target,
    parents: &[Target],
) -> Result<Admitted> {
    reject_if_refused(exclusions, carried, &target)?;
    if carried.entries.contains(&target) {
        return Ok(Admitted::AlreadyCarried { target });
    }
    doc.push_entry(|t| {
        match &target {
            Target::Branch { spec } => t["branch"] = value(spec),
            Target::Pr { number } => t["pr"] = value(*number),
            Target::Patch { path } => t["patch"] = value(path),
        }
        if !parents.is_empty() {
            t["parents"] = Item::Value(parents_array(parents).into());
        }
    });
    Ok(Admitted::Added {
        target,
        parents: parents.to_vec(),
    })
}

pub fn add(
    root: &Path,
    target: Option<String>,
    pr: Option<u64>,
    patch: Option<String>,
    parents: Vec<String>,
    prs_from: Option<String>,
) -> Result<AddReport> {
    if target.is_none() && pr.is_none() && patch.is_none() && prs_from.is_none() {
        bail!("nothing to add: pass REMOTE:BRANCH, --pr, --patch, or --prs-from");
    }
    // Parents belong to one entry, in the order that entry merged them, so
    // there has to be exactly one entry for them to belong to. A patch has no
    // history to reconstruct, and a sweep appends entries it cannot name.
    let one_entry = (target.is_some() ^ pr.is_some()) && patch.is_none() && prs_from.is_none();
    if !parents.is_empty() && !one_entry {
        bail!(
            "--parent describes what ONE entry merged in, so it applies to exactly one \
             REMOTE:BRANCH or --pr target: name that entry by itself, without --patch \
             or --prs-from"
        );
    }
    if let Some(spec) = &target {
        if !spec.contains(':') {
            bail!("branch entries are REMOTE:BRANCH (remote names come from [remotes])");
        }
    }
    let declared: Vec<Target> = parents
        .iter()
        .map(|spec| parse_parent(spec))
        .collect::<Result<_>>()?;

    let mut doc = Document::read(root)?;
    let carried = doc.carried();
    let exclusions = doc.exclusions()?;
    let mut log = Vec::new();
    let mut appended = 0usize;

    let named = [
        target.map(|spec| Target::Branch { spec }),
        pr.map(|n| Target::Pr { number: n as i64 }),
        patch.map(|path| Target::Patch { path }),
    ];
    for target in named.into_iter().flatten() {
        // Only a live entry can have parents; `one_entry` guaranteed that
        // when any were declared, the only named target is that entry.
        let admitted = admit(&mut doc, &carried, &exclusions, target, &declared)?;
        if matches!(admitted, Admitted::Added { .. }) {
            appended += 1;
        }
        log.push(admitted);
    }
    if let Some(author) = prs_from {
        let slug = doc.base_repo_slug()?;
        let prs = open_prs_by(&slug, &author)?;
        if prs.is_empty() {
            log.push(Admitted::NoOpenPrs { author, slug });
        }
        for (number, title, verdict) in
            triage_discovered(&prs, &carried.entries, &carried.parents, &exclusions)
        {
            if verdict == Discovered::Append {
                doc.push_entry(|t| {
                    t["pr"] = value(number);
                    t["summary"] = value(&title);
                });
                appended += 1;
            }
            log.push(Admitted::Discovered {
                number,
                title,
                verdict,
            });
        }
    }

    if appended > 0 {
        doc.save()?;
    }
    Ok(AddReport { log, appended })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found() -> Vec<(i64, String)> {
        vec![
            (7, "superseded".to_string()),
            (8, "carried".to_string()),
            (9, "fresh".to_string()),
        ]
    }

    fn pr(number: i64) -> Target {
        Target::Pr { number }
    }

    fn excluding(number: i64, reason: Option<&str>) -> Exclusion {
        Exclusion {
            target: pr(number),
            reason: reason.map(str::to_string),
        }
    }

    fn excluded(reason: &str) -> Discovered {
        Discovered::Excluded {
            reason: reason.into(),
        }
    }

    fn carried_as_parent(entry: &str) -> Discovered {
        Discovered::CarriedAsParent {
            entry: entry.into(),
        }
    }

    /// The sweep's whole job: an excluded PR is skipped with its reason while
    /// everything else it found is still appended.
    #[test]
    fn discovery_skips_excluded_prs_and_appends_the_rest() {
        let carried = HashSet::from([pr(8)]);
        let excludes = vec![excluding(7, Some("superseded by 10"))];
        let verdicts = triage_discovered(&found(), &carried, &HashMap::new(), &excludes);
        assert_eq!(
            verdicts.iter().map(|(n, _, v)| (*n, v)).collect::<Vec<_>>(),
            vec![
                (7, &excluded("superseded by 10")),
                (8, &Discovered::AlreadyCarried),
                (9, &Discovered::Append),
            ]
        );
    }

    /// An exclusion without a reason still bites; only the message changes.
    #[test]
    fn discovery_skips_an_exclusion_with_no_reason() {
        let verdicts = triage_discovered(
            &found(),
            &HashSet::new(),
            &HashMap::new(),
            &[excluding(7, None)],
        );
        assert_eq!(verdicts[0].2, excluded("no reason recorded"));
    }

    /// Carried wins the report when a target is somehow both: the manifest
    /// load rejects that state, so the sweep never needs to arbitrate it.
    #[test]
    fn discovery_reports_carried_before_excluded() {
        let verdicts = triage_discovered(
            &found(),
            &HashSet::from([pr(7)]),
            &HashMap::new(),
            &[excluding(7, Some("stale"))],
        );
        assert_eq!(verdicts[0].2, Discovered::AlreadyCarried);
    }

    /// A PR some entry merged in is already carried, inside that entry. The
    /// sweep must skip it without an exclusion: the `parents` declaration is
    /// the record of why it stays out, and naming the carrier is what makes
    /// the skip auditable.
    #[test]
    fn discovery_skips_prs_carried_as_parents() {
        let parents = HashMap::from([(pr(7), "combined".to_string())]);
        let verdicts = triage_discovered(&found(), &HashSet::new(), &parents, &[]);
        assert_eq!(
            verdicts.iter().map(|(n, _, v)| (*n, v)).collect::<Vec<_>>(),
            vec![
                (7, &carried_as_parent("combined")),
                (8, &Discovered::Append),
                (9, &Discovered::Append),
            ]
        );
    }

    /// A parent declaration is more specific than an exclusion, so it is what
    /// the sweep reports when a manifest somehow carries both.
    #[test]
    fn a_parent_declaration_outranks_an_exclusion_in_the_report() {
        let parents = HashMap::from([(pr(7), "combined".to_string())]);
        let verdicts = triage_discovered(
            &found(),
            &HashSet::new(),
            &parents,
            &[excluding(7, Some("stale"))],
        );
        assert_eq!(verdicts[0].2, carried_as_parent("combined"));
    }
}
