//! The provenance record: what the assembled branch was built from, written
//! into the tree as the build's last commit when the manifest asks for one.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{json, Value};

use super::Ctx;
use crate::git;
use crate::lock::EntryResult;
use crate::manifest::{self, Kind};
use crate::state::State;

pub fn provenance_json(ctx: &Ctx, st: &State, base: &str) -> Result<Value> {
    let m = &ctx.manifest;
    let subject = git::out(&ctx.repo, &["log", "-1", "--format=%s", base])?;
    let date = git::out(&ctx.repo, &["log", "-1", "--format=%cI", base])?;

    let results: BTreeMap<&str, &EntryResult> =
        st.results.iter().map(|r| (r.name.as_str(), r)).collect();
    let entries: Vec<Value> = m
        .entries
        .iter()
        .map(|entry| {
            let result = results.get(entry.name.as_str());
            let mut record = json!({
                "label": entry.name,
                "kind": entry.kind.kind_str(),
                "status": result.map(|r| r.status.as_str()).unwrap_or("unknown"),
                "commit": result.map(|r| r.oid.as_str()).unwrap_or(""),
            });
            let obj = record.as_object_mut().expect("record is an object");
            if let Some(pr) = entry.pr_number() {
                obj.insert("pr".into(), json!(pr));
            }
            if let Kind::Branch { branch, .. } = &entry.kind {
                obj.insert("branch".into(), json!(branch));
            }
            if let Some(fixup) = &entry.fixup {
                obj.insert("fixup".into(), json!(fixup));
            }
            // A derived entry's `commit` is its pin, which by itself explains
            // none of the tree it contributed. The parents and the two
            // reconstruction commits are what make that tree accountable.
            if entry.is_derived() {
                let pins = st.parent_pins.get(&entry.name);
                let parents: Vec<Value> = entry
                    .parents
                    .iter()
                    .map(|parent| {
                        json!({
                            "label": parent.name,
                            "source": parent.source(),
                            "commit": pins
                                .and_then(|pins| pins.get(&parent.name))
                                .cloned()
                                .unwrap_or_default(),
                        })
                    })
                    .collect();
                obj.insert("parents".into(), json!(parents));
                if let Some(derived) = result.and_then(|r| r.derived.as_ref()) {
                    obj.insert(
                        "derived".into(),
                        json!({ "baseTip": derived.base_tip, "tip": derived.tip }),
                    );
                }
            }
            if let Some(summary) = &entry.summary {
                obj.insert("summary".into(), json!(summary));
            }
            if let Some(note) = &entry.note {
                obj.insert("note".into(), json!(note.trim()));
            }
            record
        })
        .collect();

    let mut top = json!({
        "schemaVersion": 1,
        "manifest": manifest::FILE,
        "upstream": {
            "remote": m.remote_url(&m.base.remote)?,
            "ref": m.base.ref_,
            "commit": base,
            "subject": subject,
            "date": date,
        },
        "entries": entries,
    });
    if let Some(publish) = &m.publish {
        let remote_url = publish
            .remote
            .as_deref()
            .and_then(|name| m.remotes.get(name).cloned());
        top.as_object_mut().expect("top is an object").insert(
            "fork".into(),
            json!({
                "remote": remote_url,
                "branch": publish.branch,
            }),
        );
    }
    Ok(top)
}
