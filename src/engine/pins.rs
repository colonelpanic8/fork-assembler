//! Fetching live refs and resolving the pins a build consumes.
//!
//! `build` never moves an existing pin (`update` is the only verb that does);
//! entries with no pin yet get pinned from live refs on their first build.
//! Parents obey the same rule.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};

use super::Ctx;
use crate::git;
use crate::manifest::{self, Entry, Kind, Parent};
use crate::report::Event;

/// A private ref namespace so fetched heads never collide with user refs.
fn holding_ref(entry: &Entry) -> String {
    format!(
        "refs/fork-assembler/{}",
        manifest::sanitize_name(&entry.name)
    )
}

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

/// Fetch `kind`'s live ref into `holding` and return what it resolved to.
fn fetch_ref(ctx: &Ctx, kind: &Kind, holding: &str) -> Result<String> {
    let Some((remote, source)) = kind.remote_ref() else {
        bail!("{} is a patch, not a fetchable ref", kind.source());
    };
    let spec = format!("+{source}:{holding}");
    git::out(&ctx.repo, &["fetch", remote, &spec])?;
    git::out(&ctx.repo, &["rev-parse", holding])
}

pub fn fetch_entry(ctx: &Ctx, entry: &Entry) -> Result<String> {
    match &entry.kind {
        Kind::Patch { path } => patch_blob(&ctx.root, path),
        kind => fetch_ref(ctx, kind, &holding_ref(entry)),
    }
}

pub fn fetch_parent(ctx: &Ctx, entry: &Entry, parent: &Parent) -> Result<String> {
    fetch_ref(ctx, &parent.kind, &parent_holding_ref(entry, parent))
}

pub fn fetch_base(ctx: &Ctx) -> Result<String> {
    let base = &ctx.manifest.base;
    let spec = format!("+refs/heads/{}:refs/fork-assembler/base", base.ref_);
    git::out(&ctx.repo, &["fetch", &base.remote, &spec])?;
    git::out(&ctx.repo, &["rev-parse", "refs/fork-assembler/base"])
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

/// Make sure a pinned commit is in the object store, fetching its live ref
/// once when permitted. `what` names the pin for the two refusals.
fn ensure_present(
    ctx: &Ctx,
    pin: &str,
    locked: bool,
    what: &str,
    entry: &str,
    fetch: impl FnOnce() -> Result<String>,
) -> Result<()> {
    if git::has_commit(&ctx.repo, pin) {
        return Ok(());
    }
    if locked {
        bail!("{what} is pinned at {pin}, which is not present locally, and --locked forbids fetching");
    }
    fetch()?;
    if !git::has_commit(&ctx.repo, pin) {
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
    ctx: &Ctx,
    entry: &Entry,
    pins: &mut BTreeMap<String, String>,
    locked: bool,
) -> Result<String> {
    if let Some(pin) = pins.get(&entry.name) {
        let pin = pin.clone();
        match &entry.kind {
            Kind::Patch { path } => {
                if patch_blob(&ctx.root, path)? != pin {
                    bail!(
                        "{}: patch file content changed since it was pinned; run `fork-assembler update {}`",
                        entry.name,
                        entry.name
                    );
                }
            }
            _ => ensure_present(ctx, &pin, locked, &entry.name, &entry.name, || {
                fetch_entry(ctx, entry)
            })?,
        }
        return Ok(pin);
    }
    if locked {
        bail!(
            "{}: no pin recorded and --locked forbids pinning; run `fork-assembler build` or `update` first",
            entry.name
        );
    }
    let oid = fetch_entry(ctx, entry)?;
    ctx.emit(Event::Pinned {
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
    ctx: &Ctx,
    entry: &Entry,
    parent_pins: &mut BTreeMap<String, BTreeMap<String, String>>,
    locked: bool,
) -> Result<()> {
    let mut pins = parent_pins.get(&entry.name).cloned().unwrap_or_default();
    for parent in &entry.parents {
        let what = format!("{}: parent {}", entry.name, parent.name);
        match pins.get(&parent.name).cloned() {
            Some(pin) => ensure_present(ctx, &pin, locked, &what, &entry.name, || {
                fetch_parent(ctx, entry, parent)
            })?,
            None => {
                if locked {
                    bail!(
                        "{what} has no pin recorded and --locked forbids pinning; \
                         run `fork-assembler build` or `update {}` first",
                        entry.name
                    );
                }
                let oid = fetch_parent(ctx, entry, parent)?;
                ctx.emit(Event::PinnedParent {
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
