//! Launch declarations are remote-facing. Workstation source paths and
//! inheritance never travel in these types.

use crate::ManifestParam;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Pre,
    #[default]
    Post,
}
impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pre => "pre",
            Self::Post => "post",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Shell {
    pub program: Option<String>,
    pub path: Option<Vec<String>>,
    pub change_login_shell: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileMode {
    #[default]
    Copy,
    Link,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Overwrite {
    #[default]
    Deny,
    Backup,
    Replace,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rerun {
    #[default]
    Manual,
    OnChange,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchPlan {
    pub schema_version: u32,
    pub shell: Shell,
    pub env: BTreeMap<String, String>,
    pub artifacts: Vec<Artifact>,
    pub nodes: Vec<Node>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub id: String,
    pub phase: Phase,
    pub directory: bool,
    pub exclude: Vec<String>,
    pub used_by: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub phase: Phase,
    pub after: Vec<String>,
    pub resources: Vec<String>,
    pub action: Action,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Action {
    Condition,
    Runtime,
    Packages {
        packages: Vec<String>,
    },
    File {
        artifact: String,
        target: String,
        mode: FileMode,
        overwrite: Overwrite,
    },
    Script {
        artifact: String,
        entrypoint: String,
        interpreter: Vec<String>,
        args: Vec<String>,
        working_dir: Option<String>,
        env: BTreeMap<String, String>,
        check: Option<Vec<String>>,
        rerun: Rerun,
    },
    Builtin {
        template: String,
        params: BTreeMap<String, ManifestParam>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    pub schema_version: u32,
    pub launch_id: String,
    pub plan_digest: String,
    pub plan: LaunchPlan,
}

/// How a daemon launch's `interface-ready` is released.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Gate {
    /// Once the initial shell and the TUI report ready (or by an explicit
    /// signal).
    #[default]
    Interactive,
    /// As soon as pre succeeds: explicit headless initialization.
    Headless,
}

/// A daemon launch's lifecycle.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LaunchStatus {
    Accepted,
    Running,
    Ok,
    Failed,
    /// The daemon stopped while it was running; a retry resumes it.
    Interrupted,
}

impl LaunchStatus {
    pub fn finished(self) -> bool {
        matches!(self, Self::Ok | Self::Failed | Self::Interrupted)
    }
}

/// What a launch upload carries: the declaration (`launch.json`) or one
/// phase's bundle archive.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Blob {
    Declaration,
    Pre,
    Post,
}

impl Blob {
    pub fn name(self) -> &'static str {
        match self {
            Self::Declaration => "declaration",
            Self::Pre => "pre",
            Self::Post => "post",
        }
    }

    pub fn phase(self) -> Option<Phase> {
        match self {
            Self::Declaration => None,
            Self::Pre => Some(Phase::Pre),
            Self::Post => Some(Phase::Post),
        }
    }

    pub fn of(phase: Phase) -> Self {
        match phase {
            Phase::Pre => Self::Pre,
            Phase::Post => Self::Post,
        }
    }
}

/// Raw bytes per `LaunchUploadChunk` (base64 adds a third; the frame stays
/// well under `MAX_FRAME_BYTES`).
pub const UPLOAD_CHUNK_BYTES: usize = 32 * 1024;
/// Chunks a workstation sends ahead of the Beam's acknowledgements, so other
/// bridge traffic waits behind at most this many frames.
pub const UPLOAD_WINDOW: usize = 8;
/// Largest declaration a Beam accepts.
pub const MAX_DECLARATION_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    None,
    Gzip,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BundleDescriptor {
    pub schema_version: u32,
    pub launch_id: String,
    pub plan_digest: String,
    pub phase: Phase,
    pub compression: Compression,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifest {
    pub schema_version: u32,
    pub launch_id: String,
    pub plan_digest: String,
    pub phase: Phase,
    pub artifacts: Vec<PackedArtifact>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackedArtifact {
    pub id: String,
    /// Archive-relative root: a file or a directory, never a destination.
    pub root: String,
    pub directory: bool,
    pub digest: String,
    pub entries: Vec<BundleEntry>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEntry {
    pub path: String,
    pub mode: u32,
    pub size: u64,
    pub entry: EntryKind,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum EntryKind {
    File { sha256: String },
    Directory,
    Symlink { target: String },
}
