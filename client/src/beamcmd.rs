//! Thin wrappers around the rest of `tsh beams ...` (`ls`, `rm`, `exec`,
//! `scp`, `publish`, `unpublish`, `ide`), so day-to-day beam management
//! doesn't need a separate `tsh beams` invocation with a copy-pasted beam
//! id. The one thing these add over plain `tsh beams` is *targets*: anywhere
//! tsh wants a beam id, these also accept a tracked purpose name (see
//! `state.rs`), defaulting to the `default` purpose plain `beams ssh` uses.

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};

use crate::endpoint::{Endpoint, Tsh};
use crate::state::State;
use crate::tsh::{self, BeamInfo};

/// A Beam found on the Teleport endpoint it lives on.
pub struct Located {
    pub endpoint: Endpoint,
    pub beam: BeamInfo,
    /// Every Beam listed on `endpoint` (for limits and reuse decisions).
    pub beams: Vec<BeamInfo>,
}

/// The recorded endpoint for `target`: a purpose name, or the id/uuid of a
/// tracked Beam.
fn tracked_endpoint(target: &str, state: &State) -> Option<Endpoint> {
    state
        .purposes
        .get(target)
        .or_else(|| state.purposes.values().find(|e| e.beam_id == target || e.uuid == target))
        .and_then(|entry| entry.endpoint.clone())
}

/// Resolves `target` on its own endpoint: a tracked Beam's recorded one, or
/// the current profile for anything else. Entries from before pinning that
/// turn up on the current profile adopt it (and `state` is saved).
pub fn locate(target: &str, state: &mut State) -> anyhow::Result<Located> {
    let (endpoint, pinned) = match tracked_endpoint(target, state) {
        Some(endpoint) => (endpoint, true),
        None => (Endpoint::current()?, false),
    };
    let beams = tsh::list_beams(&endpoint)?;
    let beam = match resolve(target, state, &beams) {
        Ok(beam) => beam.clone(),
        Err(err) if pinned => return Err(err.context(format!("looking on {}", endpoint.label()))),
        Err(err) => return Err(err),
    };
    if adopt_endpoint(state, &beam.uuid, &endpoint) {
        state.save()?;
    }
    Ok(Located { endpoint, beam, beams })
}

/// Records `endpoint` on every unpinned entry tracking `uuid` - once a
/// legacy entry's Beam has been seen on an endpoint, that's where it lives.
/// Returns whether anything changed.
pub fn adopt_endpoint(state: &mut State, uuid: &str, endpoint: &Endpoint) -> bool {
    let mut changed = false;
    for entry in state.purposes.values_mut().filter(|e| e.uuid == uuid && e.endpoint.is_none()) {
        entry.endpoint = Some(endpoint.clone());
        changed = true;
    }
    changed
}

/// Whether `entry` is judged against `current`'s Beam list: pinned there,
/// or from before pinning (which always followed the current profile).
fn listed_on(entry: &crate::state::PurposeEntry, current: &Endpoint) -> bool {
    entry.endpoint.as_ref().is_none_or(|endpoint| endpoint == current)
}

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
fn format_ls(state: &State, beams: &[BeamInfo], current: &Endpoint, now: DateTime<Utc>) -> String {
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
        .filter(|(_, e)| listed_on(e, current) && !beams.iter().any(|b| b.uuid == e.uuid))
        .map(|(p, e)| format!("{p} ({})", e.beam_id))
        .collect();
    if !stale.is_empty() {
        out.push_str(&format!(
            "\nstale purposes (beam gone; `beams ssh --purpose <name>` respawns, `beams rm --prune` forgets): {}\n",
            stale.join(", ")
        ));
    }
    let elsewhere: Vec<String> = state
        .purposes
        .iter()
        .filter_map(|(p, e)| {
            let endpoint = e.endpoint.as_ref().filter(|endpoint| *endpoint != current)?;
            Some(format!("{p} ({}, {})", e.beam_id, endpoint.label()))
        })
        .collect();
    if !elsewhere.is_empty() {
        out.push_str(&format!(
            "\npurposes on other clusters (not listed here; still pinned there): {}\n",
            elsewhere.join(", ")
        ));
    }
    out
}

pub fn ls(all: bool) -> anyhow::Result<u8> {
    let state = State::load()?;
    let current = Endpoint::current()?;
    let beams = if all { tsh::list_all_beams(&current)? } else { tsh::list_beams(&current)? };
    print!("{}", format_ls(&state, &beams, &current, Utc::now()));
    Ok(0)
}

/// Deletes each target's beam via `tsh beams rm` and forgets every
/// purpose tracking it. With `prune`, also forgets purposes whose beam is
/// already gone (no `tsh beams rm` needed for those).
pub fn rm(targets: &[String], prune: bool) -> anyhow::Result<u8> {
    let mut state = State::load()?;

    // Resolve everything up front so a typo in the third target doesn't
    // leave the first two already deleted.
    let mut resolved: Vec<(Endpoint, BeamInfo)> = Vec::new();
    for target in targets {
        let Located { endpoint, beam, .. } = locate(target, &mut state)?;
        if !resolved.iter().any(|(_, b)| b.uuid == beam.uuid) {
            resolved.push((endpoint, beam));
        }
    }

    let mut code = 0;
    for (endpoint, beam) in &resolved {
        let purposes = purposes_for(&state, &beam.uuid).join(",");
        let status = Tsh::pinned(endpoint)
            .command()
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
        // Only against the current profile's list, and only for purposes that
        // live there: a Beam on another cluster isn't gone just because
        // this profile can't see it.
        let current = Endpoint::current()?;
        let beams = tsh::list_beams(&current)?;
        let before: Vec<String> = state.purposes.keys().cloned().collect();
        state.purposes.retain(|_, e| !listed_on(e, &current) || beams.iter().any(|b| b.uuid == e.uuid));
        for purpose in before.iter().filter(|p| !state.purposes.contains_key(*p)) {
            eprintln!("beams: forgot stale purpose {purpose:?}");
        }
    }
    state.save()?;
    Ok(code)
}

/// Resolves `target` (defaulting to the `default` purpose) to a beam id and
/// the endpoint it lives on.
fn resolve_id(target: Option<&str>) -> anyhow::Result<(Endpoint, String)> {
    let mut state = State::load()?;
    let Located { endpoint, beam, .. } = locate(target.unwrap_or(crate::DEFAULT_PURPOSE), &mut state)?;
    Ok((endpoint, beam.id))
}

/// Runs `tsh beams <args>` against `endpoint` with inherited stdio,
/// returning its exit code.
fn passthrough(endpoint: &Endpoint, args: &[String]) -> anyhow::Result<u8> {
    let status = Tsh::pinned(endpoint)
        .command()
        .arg("beams")
        .args(args)
        .status()
        .context("running tsh beams")?;
    Ok(status.code().map_or(1, |c| c.clamp(0, 255) as u8))
}

pub fn exec(target: Option<&str>, login: &str, command: &[String]) -> anyhow::Result<u8> {
    let (endpoint, id) = resolve_id(target)?;
    let mut args = vec!["exec".to_string(), "-l".to_string(), login.to_string(), id, "--".to_string()];
    args.extend(command.iter().cloned());
    passthrough(&endpoint, &args)
}

pub fn publish(target: Option<&str>, tcp: bool) -> anyhow::Result<u8> {
    let mut args = vec!["publish".to_string()];
    if tcp {
        args.push("--tcp".to_string());
    }
    let (endpoint, id) = resolve_id(target)?;
    args.push(id);
    passthrough(&endpoint, &args)
}

pub fn unpublish(target: Option<&str>) -> anyhow::Result<u8> {
    let (endpoint, id) = resolve_id(target)?;
    passthrough(&endpoint, &["unpublish".to_string(), id])
}

pub fn ide(target: Option<&str>, ide: Option<&str>) -> anyhow::Result<u8> {
    let mut args = vec!["ide".to_string()];
    if let Some(ide) = ide {
        args.push(format!("--ide={ide}"));
    }
    let (endpoint, id) = resolve_id(target)?;
    args.push(id);
    passthrough(&endpoint, &args)
}

/// The target named by a `TARGET:PATH` scp operand (`:PATH` alone means
/// the `default` purpose), or `None` for a local path.
fn scp_target(operand: &str) -> Option<&str> {
    let (prefix, _) = operand.split_once(':')?;
    if prefix.contains('/') {
        return None;
    }
    Some(if prefix.is_empty() { crate::DEFAULT_PURPOSE } else { prefix })
}

/// Rewrites a `TARGET:PATH` scp operand's target to a beam id (`:PATH`
/// alone means the `default` purpose). An operand with no `:`, or whose
/// prefix contains a `/` (a local path like `./a:b`), is left as a local
/// path, same as scp's own rule.
fn scp_operand(operand: &str, state: &State, beams: &[BeamInfo]) -> anyhow::Result<String> {
    let Some(target) = scp_target(operand) else {
        return Ok(operand.to_string());
    };
    let (_, path) = operand.split_once(':').expect("scp_target found a ':'");
    Ok(format!("{}:{path}", resolve(target, state, beams)?.id))
}

pub fn scp(recursive: bool, quiet: bool, src: &str, dest: &str) -> anyhow::Result<u8> {
    let mut state = State::load()?;
    // `tsh beams scp` runs against one endpoint, so both remote operands
    // (if there are two) must live on the same one.
    let mut endpoint: Option<Endpoint> = None;
    let mut operands = Vec::new();
    for operand in [src, dest] {
        let Some(target) = scp_target(operand) else {
            operands.push(operand.to_string());
            continue;
        };
        let located = locate(target, &mut state)?;
        if endpoint.as_ref().is_some_and(|endpoint| *endpoint != located.endpoint) {
            bail!("both scp operands must be Beams on the same cluster");
        }
        operands.push(scp_operand(operand, &state, &located.beams)?);
        endpoint = Some(located.endpoint);
    }
    let endpoint = match endpoint {
        Some(endpoint) => endpoint,
        None => Endpoint::current()?,
    };
    let mut args = vec!["scp".to_string()];
    if recursive {
        args.push("-r".to_string());
    }
    if quiet {
        args.push("-q".to_string());
    }
    args.extend(operands);
    passthrough(&endpoint, &args)
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
                PurposeEntry {
                    beam_id: id.to_string(),
                    uuid: uuid.to_string(),
                    created_at: String::new(),
                    launch: None,
                    endpoint: None,
                },
            );
        }
        state
    }

    fn endpoint(name: &str) -> Endpoint {
        Endpoint {
            proxy: format!("{name}.example.com:443"),
            cluster: format!("{name}.example.com"),
            user: String::new(),
            teleport_home: None,
        }
    }

    #[test]
    fn ls_never_calls_another_clusters_purpose_stale() {
        let beams = vec![beam("red-fox", "u1")];
        let mut state = state(&[("default", "red-fox", "u1"), ("work", "far-away", "u7"), ("old", "gone", "u9")]);
        state.purposes.get_mut("default").unwrap().endpoint = Some(endpoint("a"));
        state.purposes.get_mut("work").unwrap().endpoint = Some(endpoint("b"));
        let now = DateTime::parse_from_rfc3339("2026-11-05T10:00:00Z").unwrap().with_timezone(&Utc);
        let out = format_ls(&state, &beams, &endpoint("a"), now);
        let stale = out.lines().find(|l| l.starts_with("stale purposes")).unwrap();
        assert!(stale.contains("old (gone)") && !stale.contains("work"), "{out}");
        let elsewhere = out.lines().find(|l| l.starts_with("purposes on other clusters")).unwrap();
        assert!(elsewhere.contains("work (far-away, b.example.com)"), "{out}");
    }

    #[test]
    fn legacy_entries_adopt_the_endpoint_their_beam_was_found_on() {
        let mut state = state(&[("default", "red-fox", "u1"), ("side", "red-fox", "u1"), ("work", "blue-owl", "u2")]);
        state.purposes.get_mut("side").unwrap().endpoint = Some(endpoint("b"));
        assert!(adopt_endpoint(&mut state, "u1", &endpoint("a")));
        assert_eq!(state.purposes["default"].endpoint, Some(endpoint("a")));
        // Already pinned, and unrelated entries, are left alone.
        assert_eq!(state.purposes["side"].endpoint, Some(endpoint("b")));
        assert_eq!(state.purposes["work"].endpoint, None);
        assert!(!adopt_endpoint(&mut state, "u1", &endpoint("a")));
    }

    #[test]
    fn tracked_targets_use_their_recorded_endpoint() {
        let mut state = state(&[("default", "red-fox", "u1"), ("legacy", "blue-owl", "u2")]);
        state.purposes.get_mut("default").unwrap().endpoint = Some(endpoint("b"));
        for target in ["default", "red-fox", "u1"] {
            assert_eq!(tracked_endpoint(target, &state), Some(endpoint("b")), "{target}");
        }
        assert_eq!(tracked_endpoint("legacy", &state), None);
        assert_eq!(tracked_endpoint("untracked", &state), None);
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
        let out = format_ls(&state, &beams, &endpoint("a"), now);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("ID"));
        assert!(lines[1].starts_with("red-fox") && lines[1].contains("default,side") && lines[1].contains("2h30m"));
        assert!(lines[2].starts_with("blue-owl") && lines[2].contains(" - "));
        assert!(out.contains("stale purposes") && out.contains("old (gone)"));
    }
}
