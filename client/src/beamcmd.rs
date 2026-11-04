//! Thin wrappers around the rest of `tsh beams ...` (`ls`, `rm`, `exec`,
//! `scp`, `publish`, `unpublish`, `ide`), so day-to-day beam management
//! doesn't need a separate `tsh beams` invocation with a copy-pasted beam
//! id. The one thing these add over plain `tsh beams` is *targets*: anywhere
//! tsh wants a beam id, these also accept a tracked purpose name (see
//! `state.rs`), defaulting to the `default` purpose plain `beams ssh` uses.

use std::process::Command;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};

use crate::state::State;
use crate::tsh::{self, BeamInfo};

/// Resolves `target` (a beam id, a beam uuid, or a tracked purpose name -
/// checked in that order) against the live beam list. A purpose whose
/// tracked beam is gone is an error naming the purpose, not "not found".
pub fn resolve<'a>(target: &str, state: &State, beams: &'a [BeamInfo]) -> anyhow::Result<&'a BeamInfo> {
    if let Some(beam) = beams.iter().find(|b| b.id == target || b.uuid == target) {
        return Ok(beam);
    }
    if let Some(entry) = state.purposes.get(target) {
        return beams.iter().find(|b| b.uuid == entry.uuid).with_context(|| {
            format!(
                "purpose {target:?} is tracked as beam {} ({}), which no longer exists",
                entry.beam_id, entry.uuid
            )
        });
    }
    if target == crate::DEFAULT_PURPOSE {
        bail!("no beam is tracked for the {target:?} purpose yet - run `beams ssh` first, or name a beam (see `beams ls`)")
    }
    bail!("no beam id, uuid, or tracked purpose matches {target:?} (see `beams ls`)")
}

/// The purposes currently tracked for `uuid`, sorted.
fn purposes_for<'a>(state: &'a State, uuid: &str) -> Vec<&'a str> {
    state.purposes.iter().filter(|(_, e)| e.uuid == uuid).map(|(p, _)| p.as_str()).collect()
}

fn remaining(expires: &str, now: DateTime<Utc>) -> String {
    match DateTime::parse_from_rfc3339(expires) {
        Ok(expires) => {
            let left = expires.signed_duration_since(now);
            if left.num_seconds() <= 0 {
                "expired".to_string()
            } else {
                format!("{}h{:02}m", left.num_hours(), left.num_minutes() % 60)
            }
        }
        Err(_) => expires.to_string(),
    }
}

/// The `beams ls` table: every live beam with its tracked purpose(s) and
/// time left, then any tracked purpose whose beam no longer exists.
fn format_ls(state: &State, beams: &[BeamInfo], now: DateTime<Utc>) -> String {
    let mut rows = vec![[
        "ID".to_string(),
        "PURPOSE".to_string(),
        "EXPIRES IN".to_string(),
        "REGION".to_string(),
        "UUID".to_string(),
    ]];
    for beam in beams {
        let purposes = purposes_for(state, &beam.uuid);
        rows.push([
            beam.id.clone(),
            if purposes.is_empty() { "-".to_string() } else { purposes.join(",") },
            remaining(&beam.expires, now),
            beam.region.clone().unwrap_or_else(|| "-".to_string()),
            beam.uuid.clone(),
        ]);
    }
    let widths: Vec<usize> = (0..5).map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0)).collect();
    let mut out = String::new();
    for row in &rows {
        let line: Vec<String> = row.iter().zip(&widths).map(|(cell, w)| format!("{cell:<w$}")).collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    let stale: Vec<String> = state
        .purposes
        .iter()
        .filter(|(_, e)| !beams.iter().any(|b| b.uuid == e.uuid))
        .map(|(p, e)| format!("{p} ({})", e.beam_id))
        .collect();
    if !stale.is_empty() {
        out.push_str(&format!(
            "\nstale purposes (beam gone; `beams ssh --purpose <name>` respawns, `beams rm --prune` forgets): {}\n",
            stale.join(", ")
        ));
    }
    out
}

pub fn ls(all: bool) -> anyhow::Result<u8> {
    let state = State::load()?;
    let beams = if all { tsh::list_all_beams()? } else { tsh::list_beams()? };
    print!("{}", format_ls(&state, &beams, Utc::now()));
    Ok(0)
}

/// Deletes each target's beam via `tsh beams rm` and forgets every
/// purpose tracking it. With `prune`, also forgets purposes whose beam is
/// already gone (no `tsh beams rm` needed for those).
pub fn rm(targets: &[String], prune: bool) -> anyhow::Result<u8> {
    let mut state = State::load()?;
    let beams = tsh::list_beams()?;

    // Resolve everything up front so a typo in the third target doesn't
    // leave the first two already deleted.
    let mut resolved: Vec<BeamInfo> = Vec::new();
    for target in targets {
        let beam = resolve(target, &state, &beams)?;
        if !resolved.iter().any(|b| b.uuid == beam.uuid) {
            resolved.push(beam.clone());
        }
    }

    let mut code = 0;
    for beam in &resolved {
        let purposes = purposes_for(&state, &beam.uuid).join(",");
        let status = Command::new("tsh")
            .args(["beams", "rm", &beam.id])
            .status()
            .context("running tsh beams rm")?;
        if !status.success() {
            eprintln!("beams: tsh beams rm {} failed ({status})", beam.id);
            code = 1;
            continue;
        }
        if purposes.is_empty() {
            eprintln!("beams: removed {}", beam.id);
        } else {
            eprintln!("beams: removed {} (was purpose {purposes})", beam.id);
        }
        state.purposes.retain(|_, e| e.uuid != beam.uuid);
    }

    if prune {
        let before: Vec<String> = state.purposes.keys().cloned().collect();
        state.purposes.retain(|_, e| beams.iter().any(|b| b.uuid == e.uuid));
        for purpose in before.iter().filter(|p| !state.purposes.contains_key(*p)) {
            eprintln!("beams: forgot stale purpose {purpose:?}");
        }
    }
    state.save()?;
    Ok(code)
}

/// Resolves `target` (defaulting to the `default` purpose) to a beam id.
fn resolve_id(target: Option<&str>) -> anyhow::Result<String> {
    let state = State::load()?;
    let beams = tsh::list_beams()?;
    Ok(resolve(target.unwrap_or(crate::DEFAULT_PURPOSE), &state, &beams)?.id.clone())
}

/// Runs `tsh beams <args>` with inherited stdio, returning its exit code.
fn passthrough(args: &[String]) -> anyhow::Result<u8> {
    let status = Command::new("tsh").arg("beams").args(args).status().context("running tsh beams")?;
    Ok(status.code().map_or(1, |c| c.clamp(0, 255) as u8))
}

pub fn exec(target: Option<&str>, login: &str, command: &[String]) -> anyhow::Result<u8> {
    let id = resolve_id(target)?;
    let mut args = vec!["exec".to_string(), "-l".to_string(), login.to_string(), id, "--".to_string()];
    args.extend(command.iter().cloned());
    passthrough(&args)
}

pub fn publish(target: Option<&str>, tcp: bool) -> anyhow::Result<u8> {
    let mut args = vec!["publish".to_string()];
    if tcp {
        args.push("--tcp".to_string());
    }
    args.push(resolve_id(target)?);
    passthrough(&args)
}

pub fn unpublish(target: Option<&str>) -> anyhow::Result<u8> {
    passthrough(&["unpublish".to_string(), resolve_id(target)?])
}

pub fn ide(target: Option<&str>, ide: Option<&str>) -> anyhow::Result<u8> {
    let mut args = vec!["ide".to_string()];
    if let Some(ide) = ide {
        args.push(format!("--ide={ide}"));
    }
    args.push(resolve_id(target)?);
    passthrough(&args)
}

/// Rewrites a `TARGET:PATH` scp operand's target to a beam id (`:PATH`
/// alone means the `default` purpose). An operand with no `:`, or whose
/// prefix contains a `/` (a local path like `./a:b`), is left as a local
/// path, same as scp's own rule.
fn scp_operand(operand: &str, state: &State, beams: &[BeamInfo]) -> anyhow::Result<String> {
    let Some((prefix, path)) = operand.split_once(':') else {
        return Ok(operand.to_string());
    };
    if prefix.contains('/') {
        return Ok(operand.to_string());
    }
    let target = if prefix.is_empty() { crate::DEFAULT_PURPOSE } else { prefix };
    Ok(format!("{}:{path}", resolve(target, state, beams)?.id))
}

pub fn scp(recursive: bool, quiet: bool, src: &str, dest: &str) -> anyhow::Result<u8> {
    let state = State::load()?;
    let beams = tsh::list_beams()?;
    let mut args = vec!["scp".to_string()];
    if recursive {
        args.push("-r".to_string());
    }
    if quiet {
        args.push("-q".to_string());
    }
    args.push(scp_operand(src, &state, &beams)?);
    args.push(scp_operand(dest, &state, &beams)?);
    passthrough(&args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PurposeEntry;

    fn beam(id: &str, uuid: &str) -> BeamInfo {
        BeamInfo {
            id: id.to_string(),
            uuid: uuid.to_string(),
            owner: "me".to_string(),
            expires: "2026-11-05T12:30:00Z".to_string(),
            region: Some("us-west-2".to_string()),
        }
    }

    fn state(entries: &[(&str, &str, &str)]) -> State {
        let mut state = State::default();
        for (purpose, id, uuid) in entries {
            state.purposes.insert(
                purpose.to_string(),
                PurposeEntry { beam_id: id.to_string(), uuid: uuid.to_string(), created_at: String::new() },
            );
        }
        state
    }

    #[test]
    fn resolve_accepts_id_uuid_or_purpose() {
        let beams = vec![beam("red-fox", "u1"), beam("blue-owl", "u2")];
        let state = state(&[("default", "blue-owl", "u2")]);
        assert_eq!(resolve("red-fox", &state, &beams).unwrap().id, "red-fox");
        assert_eq!(resolve("u1", &state, &beams).unwrap().id, "red-fox");
        assert_eq!(resolve("default", &state, &beams).unwrap().id, "blue-owl");
    }

    #[test]
    fn resolve_explains_a_purpose_whose_beam_is_gone() {
        let beams = vec![beam("red-fox", "u1")];
        let state = state(&[("side", "old-beam", "u9")]);
        let err = resolve("side", &state, &beams).unwrap_err().to_string();
        assert!(err.contains("no longer exists"), "{err}");
        let err = resolve("nope", &state, &beams).unwrap_err().to_string();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn scp_operands_resolve_targets_and_leave_local_paths_alone() {
        let beams = vec![beam("red-fox", "u1")];
        let state = state(&[("default", "red-fox", "u1")]);
        assert_eq!(scp_operand("default:/tmp/x", &state, &beams).unwrap(), "red-fox:/tmp/x");
        assert_eq!(scp_operand(":notes.txt", &state, &beams).unwrap(), "red-fox:notes.txt");
        assert_eq!(scp_operand("u1:a", &state, &beams).unwrap(), "red-fox:a");
        assert_eq!(scp_operand("local.txt", &state, &beams).unwrap(), "local.txt");
        assert_eq!(scp_operand("./odd:name", &state, &beams).unwrap(), "./odd:name");
        assert!(scp_operand("missing:x", &state, &beams).is_err());
    }

    #[test]
    fn ls_shows_purposes_time_left_and_stale_entries() {
        let beams = vec![beam("red-fox", "u1"), beam("blue-owl", "u2")];
        let state = state(&[("default", "red-fox", "u1"), ("side", "red-fox", "u1"), ("old", "gone", "u9")]);
        let now = DateTime::parse_from_rfc3339("2026-11-05T10:00:00Z").unwrap().with_timezone(&Utc);
        let out = format_ls(&state, &beams, now);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("ID"));
        assert!(lines[1].starts_with("red-fox") && lines[1].contains("default,side") && lines[1].contains("2h30m"));
        assert!(lines[2].starts_with("blue-owl") && lines[2].contains(" - "));
        assert!(out.contains("stale purposes") && out.contains("old (gone)"));
    }
}
