//! Tracked git-rerere conflict resolutions.
//!
//! The single resolution mechanism is git rerere with repo-tracked storage:
//!
//! ```text
//! resolutions/rerere/<conflict-hash>/preimage     git's own rr-cache format
//! resolutions/rerere/<conflict-hash>/postimage
//! resolutions/rerere/INDEX.toml                   informational index
//! ```
//!
//! Every build seeds the build worktree's rr-cache EXCLUSIVELY from the
//! tracked pairs (wipe, then copy), so nothing ambient ever leaks in and the
//! operator's own rr-cache is never consulted. rerere is enabled only
//! per-command via `-c` flags during build operations — never persistently.
//!
//! `continue` harvests new or updated pairs from the worktree rr-cache back
//! into `resolutions/rerere/` after committing a manual resolution, and
//! records which entry produced them in INDEX.toml. The index is for
//! auditability only, never load-bearing.
//!
//! Each entry's lock result lists the pairs its step replayed or recorded,
//! read from postimage mtimes (see `take_used`); `gc` removes the pairs no
//! result lists.
//!
//! Known limitation, by design: rerere pairs capture only conflicted-hunk
//! resolutions. Edits made outside conflict hunks while resolving, and
//! conflicts rerere cannot token-ize (delete/modify, binary), are not
//! captured — such content belongs in patch entries. The end-of-build lock
//! tree hash remains the sole verification invariant.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::git;

pub const DIR: &str = "resolutions/rerere";
pub const INDEX: &str = "resolutions/rerere/INDEX.toml";

/// Per-command config enabling rerere for build operations only.
const CFG: [&str; 4] = ["-c", "rerere.enabled=true", "-c", "rerere.autoUpdate=true"];

/// Prefix `args` with the per-command rerere config.
pub fn with_cfg<'a>(args: &[&'a str]) -> Vec<&'a str> {
    CFG.iter().copied().chain(args.iter().copied()).collect()
}

/// The mtime every seeded postimage carries until git uses it. git touches a
/// postimage when it replays that resolution and writes a fresh one when it
/// records a new one, so any other mtime marks a pair used since. 2100-01-01,
/// far in the future, because `git rerere gc` expires resolutions by
/// postimage age.
const UNUSED: Duration = Duration::from_secs(4_102_444_800);

/// The rr-cache directory the worktree's git uses. rr-cache lives in the
/// COMMON git dir, shared by all worktrees of the source repo.
fn rr_cache(worktree: &Path) -> Result<PathBuf> {
    let path = git::out(
        worktree,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "rr-cache",
        ],
    )?;
    Ok(PathBuf::from(path))
}

/// True for the rr-cache files that constitute a durable pair. `thisimage`
/// and friends are transient working state and never tracked.
fn is_pair_file(name: &str) -> bool {
    name.starts_with("preimage") || is_postimage(name)
}

fn is_postimage(name: &str) -> bool {
    name.starts_with("postimage")
}

fn copy_pair_files(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for file in fs::read_dir(src)? {
        let file = file?;
        let name = file.file_name().to_string_lossy().to_string();
        if is_pair_file(&name) {
            fs::copy(file.path(), dst.join(&name))
                .with_context(|| format!("copying {}", file.path().display()))?;
        }
    }
    Ok(())
}

/// True when the pair files (names + bytes) in `a` differ from those in `b`.
fn pairs_differ(a: &Path, b: &Path) -> Result<bool> {
    let list = |dir: &Path| -> Result<BTreeMap<String, Vec<u8>>> {
        let mut files = BTreeMap::new();
        if dir.exists() {
            for file in fs::read_dir(dir)? {
                let file = file?;
                let name = file.file_name().to_string_lossy().to_string();
                if is_pair_file(&name) {
                    files.insert(name, fs::read(file.path())?);
                }
            }
        }
        Ok(files)
    };
    Ok(list(a)? != list(b)?)
}

/// The rr-cache's postimage files, by conflict hash.
fn postimages(cache: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    if !cache.exists() {
        return Ok(files);
    }
    for entry in fs::read_dir(cache)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let hash = entry.file_name().to_string_lossy().to_string();
        for file in fs::read_dir(entry.path())? {
            let file = file?;
            if is_postimage(&file.file_name().to_string_lossy()) {
                files.push((hash.clone(), file.path()));
            }
        }
    }
    Ok(files)
}

fn mark_unused(postimage: &Path) -> Result<()> {
    fs::File::options()
        .write(true)
        .open(postimage)
        .and_then(|file| file.set_modified(UNIX_EPOCH + UNUSED))
        .with_context(|| format!("resetting the mtime of {}", postimage.display()))
}

/// Wipe the worktree's rr-cache and reseed it exclusively from the tracked
/// pairs, every one marked unused. Returns the number of seeded conflict
/// entries.
pub fn seed(root: &Path, worktree: &Path) -> Result<usize> {
    let cache = rr_cache(worktree)?;
    if cache.exists() {
        fs::remove_dir_all(&cache).with_context(|| format!("clearing {}", cache.display()))?;
    }
    fs::create_dir_all(&cache)?;
    let tracked = root.join(DIR);
    let mut seeded = 0usize;
    if tracked.exists() {
        for entry in fs::read_dir(&tracked)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                copy_pair_files(&entry.path(), &cache.join(entry.file_name()))?;
                seeded += 1;
            }
        }
    }
    for (_, postimage) in postimages(&cache)? {
        mark_unused(&postimage)?;
    }
    Ok(seeded)
}

/// The conflict hashes whose pairs git replayed or recorded since the last
/// call (or the seed), sorted. Marks them unused again, so each call answers
/// for one step and a pair two steps replay counts for both.
pub fn take_used(worktree: &Path) -> Result<Vec<String>> {
    let mut used = BTreeSet::new();
    for (hash, postimage) in postimages(&rr_cache(worktree)?)? {
        if fs::metadata(&postimage)?.modified()? != UNIX_EPOCH + UNUSED {
            mark_unused(&postimage)?;
            used.insert(hash);
        }
    }
    Ok(used.into_iter().collect())
}

/// The conflicts rerere is tracking for the in-progress merge:
/// (conflict hash, path). Parsed from MERGE_RR (NUL-terminated
/// `<hash>[.<variant>]\t<path>` records); best-effort.
pub fn merge_rr(worktree: &Path) -> Result<Vec<(String, String)>> {
    let path = git::out(
        worktree,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "MERGE_RR",
        ],
    )?;
    let path = PathBuf::from(path);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(&path)?;
    let mut records = Vec::new();
    for record in bytes.split(|b| *b == 0) {
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(2, |b| *b == b'\t');
        let (Some(hash), Some(file)) = (fields.next(), fields.next()) else {
            continue;
        };
        let hash = String::from_utf8_lossy(hash);
        // A variant suffix (`<hash>.N`) selects a file inside the entry's
        // directory; the directory itself is named by the bare hash.
        let hash = hash.split('.').next().unwrap_or(&hash).to_string();
        records.push((hash, String::from_utf8_lossy(file).to_string()));
    }
    Ok(records)
}

/// Copy every rr-cache entry that has a postimage and is new or changed
/// relative to the tracked pairs into `resolutions/rerere/`. The cache was
/// seeded exclusively from tracked pairs, so anything that differs was
/// produced by this build's resolutions. Returns the harvested hashes.
pub fn harvest(root: &Path, worktree: &Path) -> Result<Vec<String>> {
    let cache = rr_cache(worktree)?;
    let mut harvested = Vec::new();
    if !cache.exists() {
        return Ok(harvested);
    }
    for entry in fs::read_dir(&cache)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let has_postimage = fs::read_dir(entry.path())?
            .filter_map(|f| f.ok())
            .any(|f| is_postimage(&f.file_name().to_string_lossy()));
        if !has_postimage {
            continue;
        }
        let hash = entry.file_name().to_string_lossy().to_string();
        let tracked = root.join(DIR).join(&hash);
        if pairs_differ(&entry.path(), &tracked)? {
            copy_pair_files(&entry.path(), &tracked)?;
            harvested.push(hash);
        }
    }
    harvested.sort();
    Ok(harvested)
}

#[derive(Serialize, Deserialize, Default)]
struct Index {
    #[serde(default, rename = "resolution")]
    resolutions: Vec<IndexEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct IndexEntry {
    /// Manifest entry whose conflicted merge produced this pair.
    entry: String,
    /// rr-cache conflict hash (directory name under resolutions/rerere/).
    hash: String,
    /// Files the conflict occurred in, when known.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<String>,
    recorded: String,
}

fn load_index(root: &Path) -> Result<Index> {
    let path = root.join(INDEX);
    if !path.exists() {
        return Ok(Index::default());
    }
    toml::from_str(&fs::read_to_string(&path)?)
        .with_context(|| format!("parsing {}", path.display()))
}

/// Record which entry produced the given harvested pairs. Informational
/// only — never consulted for replay.
pub fn index_add(
    root: &Path,
    entry_name: &str,
    hashes: &[String],
    paths_by_hash: &[(String, String)],
) -> Result<()> {
    if hashes.is_empty() {
        return Ok(());
    }
    let mut index = load_index(root)?;
    index.resolutions.retain(|r| !hashes.contains(&r.hash));
    for hash in hashes {
        let paths: Vec<String> = paths_by_hash
            .iter()
            .filter(|(h, _)| h == hash)
            .map(|(_, p)| p.clone())
            .collect();
        index.resolutions.push(IndexEntry {
            entry: entry_name.to_string(),
            hash: hash.clone(),
            paths,
            recorded: chrono::Utc::now().to_rfc3339(),
        });
    }
    index
        .resolutions
        .sort_by(|a, b| a.entry.cmp(&b.entry).then_with(|| a.hash.cmp(&b.hash)));
    let mut body = String::from(
        "# Informational index of tracked rerere pairs: which entry's merge\n\
         # produced which conflict hash. Auditability only, never load-bearing;\n\
         # replay uses the pair directories, verification uses the lock tree.\n",
    );
    body.push_str(&toml::to_string(&index)?);
    fs::create_dir_all(root.join(DIR))?;
    fs::write(root.join(INDEX), body)?;
    Ok(())
}

/// Entry names with at least one recorded pair, for status reporting.
pub fn index_entry_names(root: &Path) -> Result<BTreeSet<String>> {
    Ok(load_index(root)?
        .resolutions
        .into_iter()
        .map(|r| r.entry)
        .collect())
}
