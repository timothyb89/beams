use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Tracks which beam serves which "purpose" (`default` for plain `beams
/// ssh`, anything else for beams spawned ad hoc for a specific task). The
/// Beams API has no server-side labeling, so this is the only way to tell
/// "the beam `beams ssh` should reuse" apart from one-off beams spawned for
/// something else.
#[derive(Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub purposes: BTreeMap<String, PurposeEntry>,
}

#[derive(Serialize, Deserialize)]
pub struct PurposeEntry {
    pub beam_id: String,
    pub uuid: String,
    pub created_at: String,
    /// The launch the Beam runs (`beams ssh`). Absent only in entries from
    /// before profile launches, which `beams ssh` no longer reuses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<PurposeLaunch>,
    /// The Teleport proxy/cluster the Beam was created on; every `tsh` call
    /// for it is pinned there (`endpoint.rs`). Absent in entries from before
    /// pinning: those follow the current profile until first seen there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<crate::endpoint::Endpoint>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PurposeLaunch {
    pub launch_id: String,
    /// The profile as given (`builtin:development`, an absolute path).
    pub profile: String,
    pub plan_digest: String,
    /// The packed launch on this workstation (the local daemon's driver
    /// reads post sources and blobs from it).
    pub launch_dir: PathBuf,
}

impl State {
    pub fn load() -> anyhow::Result<Self> {
        let path = state_path()?;
        match fs::read_to_string(&path) {
            Ok(contents) => {
                serde_json::from_str(&contents).with_context(|| format!("parsing {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = state_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let contents =
            serde_json::to_string_pretty(self).context("serializing beams client state")?;
        fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))
    }
}

fn state_path() -> anyhow::Result<PathBuf> {
    Ok(crate::clientd::instance_dir()?.join("state.json"))
}
