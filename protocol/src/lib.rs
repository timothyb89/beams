//! Typed messages shared by the workstation `beams` daemon and the
//! in-VM `beam-bootstrap client-bridge`.
//!
//! The Beam side is allowed to submit proposals and observe status. It
//! cannot encode an approval: request decisions only exist in
//! [`ClientToBeam`], whose sender is the workstation daemon.

pub mod agent;
pub mod task_catalog;
pub mod launch;
#[cfg(feature = "agent-crypto")]
pub mod agent_crypto;

use serde::{Deserialize, Serialize};

pub const PROTOCOL: &str = "beams-client-services";
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Optional features a workstation advertises in its bridge hello. The Beam
/// only sends a message or request kind that needs a capability to bridges
/// that advertise it, so an older workstation (which would treat an unknown
/// message as a protocol error) is never sent one. Adding a capability
/// doesn't change `PROTOCOL_VERSION`.
pub const CAPABILITY_FILE_FETCH: &str = "file_fetch";

/// The workstation relays SSH agent requests (see `docs/designs/SSH_AGENT_PROXY_DESIGN.md`
/// and the `Agent*` messages below).
pub const CAPABILITY_SSH_AGENT: &str = "ssh_agent";

/// The workstation submits configured launches and uploads their bundles
/// over the bridge (`LaunchUpload*`, `LaunchSubmit`); the Beam reports their
/// status and conditions to the submitting workstation.
pub const CAPABILITY_LAUNCH: &str = "launch";

/// The workstation answers host key lookups (`KnownHostsQuery`) from its
/// own `known_hosts`, for the Beam's ssh `KnownHostsCommand`.
pub const CAPABILITY_KNOWN_HOSTS: &str = "known_hosts";

/// Every capability this build implements.
pub const CAPABILITIES: &[&str] = &[
    CAPABILITY_FILE_FETCH,
    CAPABILITY_SSH_AGENT,
    CAPABILITY_LAUNCH,
    CAPABILITY_KNOWN_HOSTS,
];

/// Longest host name a `KnownHostsQuery` may carry (`[name]:port`).
pub const MAX_KNOWN_HOST_NAME: usize = 260;
/// Most lines, and bytes in total, a `KnownHostsAnswer` may carry.
pub const MAX_KNOWN_HOSTS_LINES: usize = 64;
pub const MAX_KNOWN_HOSTS_BYTES: usize = 32 * 1024;

/// Whether `host` is a plain `KnownHostsQuery` name: a host name or
/// address, optionally `[host]:port`, with no known_hosts pattern
/// characters and no leading `-` (it ends up on a command line).
pub fn valid_known_host_name(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => match rest.split_once("]:") {
            Some((name, port)) if !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()) => name,
            _ => return false,
        },
        None => host,
    };
    !name.is_empty()
        && host.len() <= MAX_KNOWN_HOST_NAME
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_:".contains(&b))
}

/// Bounds on the advisory process context attached to an agent request, so
/// a request always fits in `MAX_FRAME_BYTES`.
pub const MAX_AGENT_CHAIN: usize = 6;
pub const MAX_AGENT_ARGV: usize = 32;
pub const MAX_AGENT_TEXT: usize = 300;

/// Most sources one fetch request may name (keeps requests well under
/// `MAX_FRAME_BYTES`).
pub const MAX_FETCH_SOURCES: usize = 200;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    PendingApproval,
    Approved,
    Rejected,
    Expired,
    Starting,
    Running,
    Failed,
}

impl RequestStatus {
    pub fn transition(self, next: Self) -> Result<Self, String> {
        let valid = matches!(
            (self, next),
            (
                Self::PendingApproval,
                Self::Approved | Self::Rejected | Self::Expired
            ) | (Self::Approved, Self::Starting | Self::Failed)
                | (Self::Starting, Self::Running | Self::Failed)
        );
        if valid {
            Ok(next)
        } else {
            Err(format!("invalid request transition {self:?} -> {next:?}"))
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Rejected | Self::Expired | Self::Running | Self::Failed
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceStatus {
    Starting,
    Running,
    Paused,
    Stopping,
    Stopped,
    Degraded,
    Unknown,
    Error,
}

impl ServiceStatus {
    pub fn transition(self, next: Self) -> Result<Self, String> {
        let valid = matches!(
            (self, next),
            (
                Self::Starting,
                Self::Running
                    | Self::Paused
                    | Self::Stopping
                    | Self::Stopped
                    | Self::Degraded
                    | Self::Unknown
                    | Self::Error,
            ) | (
                Self::Running,
                Self::Paused
                    | Self::Stopping
                    | Self::Stopped
                    | Self::Degraded
                    | Self::Unknown
                    | Self::Error
            ) | (
                Self::Degraded,
                Self::Running | Self::Paused | Self::Stopping | Self::Unknown | Self::Error
            ) | (
                Self::Paused,
                Self::Running
                    | Self::Stopping
                    | Self::Stopped
                    | Self::Degraded
                    | Self::Unknown
                    | Self::Error
            ) | (
                Self::Unknown,
                Self::Running | Self::Paused | Self::Stopping | Self::Stopped | Self::Error
            ) | (Self::Stopping, Self::Stopped | Self::Error)
                | (Self::Stopped | Self::Error, Self::Starting)
        );
        if valid {
            Ok(next)
        } else {
            Err(format!("invalid service transition {self:?} -> {next:?}"))
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAction {
    Stop,
    Restart,
    Pause,
    Resume,
    Terminate,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileCopyDirection {
    Upload,
    Download,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverwritePolicy {
    Deny,
    Replace,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FolderSyncMode {
    TwoWaySafe,
    TwoWayResolved,
    OneWaySafe,
    OneWayReplica,
}

impl FolderSyncMode {
    pub fn as_mutagen(self) -> &'static str {
        match self {
            Self::TwoWaySafe => "two-way-safe",
            Self::TwoWayResolved => "two-way-resolved",
            Self::OneWaySafe => "one-way-safe",
            Self::OneWayReplica => "one-way-replica",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ServiceActionRequest {
    pub request_id: String,
    pub service_id: String,
    pub action: ServiceAction,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ServiceRequest {
    pub request_id: String,
    pub kind: ServiceRequestKind,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServiceRequestKind {
    LocalPortForward {
        remote_host: String,
        remote_port: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_port: Option<u16>,
    },
    FileCopy {
        direction: FileCopyDirection,
        remote_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_path_hint: Option<String>,
        overwrite: OverwritePolicy,
    },
    /// Beam files and directories to copy to one workstation destination
    /// (`beams fetch`). Requires `CAPABILITY_FILE_FETCH`.
    FileFetch {
        sources: Vec<FetchSource>,
        /// A suggested destination; the workstation user confirms or edits
        /// it before approving.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_path_hint: Option<String>,
        overwrite: OverwritePolicy,
    },
    FolderSync {
        remote_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_path_hint: Option<String>,
        mode: FolderSyncMode,
        #[serde(default = "default_true")]
        ignore_vcs: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ignores: Vec<String>,
    },
}

fn default_true() -> bool {
    true
}

impl ServiceRequestKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::LocalPortForward { .. } => "local_port_forward",
            Self::FileCopy { .. } => "file_copy",
            Self::FileFetch { .. } => "file_fetch",
            Self::FolderSync { .. } => "folder_sync",
        }
    }
}

/// One `FileFetch` source as the Beam saw it when the request was made.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct FetchSource {
    /// Absolute path in the Beam.
    pub path: String,
    /// Bytes (for a directory, the total of the regular files under it).
    pub size: u64,
    #[serde(default)]
    pub is_dir: bool,
}

/// How far a transfer has got.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct TransferProgress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: usize,
    pub files_total: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ServiceProjection {
    pub service_id: String,
    pub request_id: String,
    pub kind: ServiceRequestKind,
    pub status: ServiceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TransferProgress>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RequestProjection {
    pub request: ServiceRequest,
    pub status: RequestStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ActionRequestProjection {
    pub request: ServiceActionRequest,
    pub status: RequestStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Messages emitted by the authoritative workstation daemon.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientToBeam {
    Hello {
        protocol: String,
        version: u32,
        client_id: String,
        bridge_id: String,
        session_tokens: Vec<String>,
        /// See `CAPABILITIES`; absent from workstations that predate them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
    AddSessionToken {
        session_token: String,
    },
    RequestStatus {
        request_id: String,
        status: RequestStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    RequestSnapshot {
        requests: Vec<RequestProjection>,
    },
    ActionRequestSnapshot {
        requests: Vec<ActionRequestProjection>,
    },
    ServiceSnapshot {
        services: Vec<ServiceProjection>,
    },
    ServiceStatus {
        service: ServiceProjection,
    },
    ServiceActionStatus {
        request_id: String,
        status: RequestStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Ready,
    Ping {
        nonce: String,
    },
    /// The keys the Beam-side agent may list, replacing any earlier set. An
    /// empty list means "no keys" (no frontend attached, or the proxy is
    /// disabled). Requires `CAPABILITY_SSH_AGENT`.
    AgentIdentities {
        keys: Vec<AgentKey>,
    },
    /// Starts (or resumes) uploading `blob` of launch `launch_id`: `bytes`
    /// long with SHA-256 `sha256` (lowercase hex). A bundle names its
    /// descriptor. Answered with `LaunchUploadOffset` (bytes the Beam
    /// already holds). Requires `CAPABILITY_LAUNCH`.
    LaunchUploadBegin {
        launch_id: String,
        blob: launch::Blob,
        bytes: u64,
        sha256: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        descriptor: Option<launch::BundleDescriptor>,
    },
    /// Bytes `offset..` of an upload (base64, at most
    /// `launch::UPLOAD_CHUNK_BYTES` decoded). Only the next expected offset
    /// is accepted; each is acknowledged with `LaunchUploadOffset`.
    LaunchUploadChunk {
        launch_id: String,
        blob: launch::Blob,
        offset: u64,
        data: String,
    },
    /// Accept launch `launch_id` from its uploaded declaration. Answered
    /// with `LaunchAccepted` or `LaunchRejected`; resubmitting an accepted
    /// launch (same digest) answers with its current state.
    LaunchSubmit {
        request_id: String,
        launch_id: String,
        plan_digest: String,
        #[serde(default)]
        gate: launch::Gate,
    },
    /// The reply to a `KnownHostsQuery`: matching known_hosts lines (hashed
    /// entries and `@cert-authority`/`@revoked` markers as written), none
    /// when the workstation doesn't know the host. Bounded by
    /// `MAX_KNOWN_HOSTS_LINES`/`MAX_KNOWN_HOSTS_BYTES`.
    KnownHostsAnswer {
        request_id: String,
        lines: Vec<String>,
    },
    /// The reply to an `AgentSignRequest`: a raw agent message body
    /// (`SSH_AGENT_SIGN_RESPONSE` or `SSH_AGENT_FAILURE`), base64.
    AgentResponse {
        request_id: String,
        message: String,
        /// Why it failed, for the Beam's log (the agent protocol has no way
        /// to tell the requester).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

/// A task parameter value (templates only take these three kinds).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ManifestParam {
    Bool(bool),
    Int(i64),
    Str(String),
}

/// A public key the Beam-side agent may list.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct AgentKey {
    /// Public key blob, base64.
    pub blob: String,
    pub comment: String,
}

/// Advisory context the Beam gathered about the process using the agent.
/// Everything here can be forged by anything on the Beam; the workstation
/// shows it as "reported by the Beam" and never bases a decision on it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct AgentContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The requesting process first, then its parents (at most
    /// `MAX_AGENT_CHAIN`).
    #[serde(default)]
    pub chain: Vec<AgentProcess>,
    /// Nearest `SSH_KEY_REASON` up the chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Nearest `BEAMS_TASK_RUN` up the chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_run: Option<String>,
    /// Destinations named by `ssh` command lines in the chain, spelled the
    /// way `known_hosts` would (`host` or `[host]:port`). Only ever used to
    /// *confirm* a hashed `known_hosts` entry for a verified host key.
    #[serde(default)]
    pub host_candidates: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct AgentProcess {
    pub pid: u32,
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl ClientToBeam {
    pub fn hello(
        client_id: impl Into<String>,
        bridge_id: impl Into<String>,
        session_tokens: Vec<String>,
    ) -> Self {
        Self::Hello {
            protocol: PROTOCOL.to_string(),
            version: PROTOCOL_VERSION,
            client_id: client_id.into(),
            bridge_id: bridge_id.into(),
            session_tokens,
            capabilities: CAPABILITIES
                .iter()
                .map(|capability| capability.to_string())
                .collect(),
        }
    }
}

/// Messages emitted by the Beam. There is deliberately no approval or
/// service-start variant in this direction.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BeamToClient {
    Hello {
        protocol: String,
        version: u32,
        bridge_id: String,
        /// See `CAPABILITIES`. The workstation only sends a capability's
        /// messages to Beams that list it; absent from older Beams.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
    RequestSubmitted {
        request: ServiceRequest,
    },
    ServiceActionRequested {
        request: ServiceActionRequest,
    },
    /// Whoever made the request on the Beam gave up on it (e.g. Ctrl-C in
    /// `beams fetch`): drop it if still pending, stop its one-shot service
    /// if running. Requires `CAPABILITY_FILE_FETCH`.
    RequestWithdrawn {
        request_id: String,
    },
    Pong {
        nonce: String,
    },
    Error {
        message: String,
    },
    /// A `session-bind@openssh.com` extension seen on Beam-side agent
    /// connection `conn_id` (raw agent message body, base64). The Beam has
    /// already acknowledged it; the workstation verifies it and keeps it for
    /// that connection's later sign requests. Requires `CAPABILITY_SSH_AGENT`.
    AgentSessionBind {
        conn_id: String,
        message: String,
    },
    /// An `SSH_AGENTC_SIGN_REQUEST` (raw agent message body, base64) waiting
    /// for an `AgentResponse`. Requires `CAPABILITY_SSH_AGENT`.
    AgentSignRequest {
        request_id: String,
        conn_id: String,
        message: String,
        #[serde(default)]
        context: AgentContext,
    },
    /// ssh on the Beam is checking `host` (`%H`: a name or `[name]:port`)
    /// and asks for the workstation's known_hosts entries for it. Answered
    /// with `KnownHostsAnswer`. Requires `CAPABILITY_KNOWN_HOSTS`.
    KnownHostsQuery {
        request_id: String,
        host: String,
    },
    /// Agent connection `conn_id` closed; forget its session binds.
    AgentConnClosed {
        conn_id: String,
    },
    /// How many bytes of an upload the Beam holds: the reply to
    /// `LaunchUploadBegin` and the acknowledgement of each chunk.
    LaunchUploadOffset {
        launch_id: String,
        blob: launch::Blob,
        offset: u64,
    },
    /// An upload arrived whole and matched its digest. For a bundle, the
    /// Beam then verifies and stages it (reported as `LaunchCondition`).
    LaunchUploaded {
        launch_id: String,
        blob: launch::Blob,
    },
    /// An upload was refused or failed verification; begin it again.
    LaunchUploadFailed {
        launch_id: String,
        blob: launch::Blob,
        message: String,
    },
    LaunchAccepted {
        request_id: String,
        launch_id: String,
        /// The launch had already been accepted (a resubmission).
        existing: bool,
        status: launch::LaunchStatus,
        /// Conditions released so far (`artifact-ready:pre`,
        /// `interface-ready`, ...).
        released: Vec<String>,
    },
    LaunchRejected {
        request_id: String,
        launch_id: String,
        message: String,
    },
    /// A launch's lifecycle changed. Sent to the client that submitted it.
    LaunchStatus {
        launch_id: String,
        status: launch::LaunchStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// One of a launch's external conditions was released, or failed (a
    /// bundle that didn't verify). Sent to the client that submitted it.
    LaunchCondition {
        launch_id: String,
        condition: String,
        released: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

impl BeamToClient {
    pub fn hello(bridge_id: impl Into<String>) -> Self {
        Self::Hello {
            protocol: PROTOCOL.to_string(),
            version: PROTOCOL_VERSION,
            bridge_id: bridge_id.into(),
            capabilities: CAPABILITIES.iter().map(|capability| capability.to_string()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_full_upload_chunk_fits_in_one_frame() {
        use base64ct::Encoding;
        let data = base64ct::Base64::encode_string(&vec![0xffu8; launch::UPLOAD_CHUNK_BYTES]);
        let chunk = ClientToBeam::LaunchUploadChunk {
            launch_id: "0".repeat(36),
            blob: launch::Blob::Declaration,
            offset: u64::MAX,
            data,
        };
        let line = serde_json::to_string(&chunk).unwrap();
        assert!(line.len() + 1 < MAX_FRAME_BYTES, "{} bytes", line.len());
        let window = launch::UPLOAD_WINDOW * line.len();
        assert!(window <= 512 * 1024, "the window stays small: {window}");
        assert!(CAPABILITIES.contains(&CAPABILITY_LAUNCH));
    }

    #[test]
    fn port_forward_request_has_a_stable_tagged_shape() {
        let request = ServiceRequest {
            request_id: "request-1".to_string(),
            kind: ServiceRequestKind::LocalPortForward {
                remote_host: "127.0.0.1".to_string(),
                remote_port: 8080,
                local_port: Some(18080),
            },
        };
        assert_eq!(
            serde_json::to_value(BeamToClient::RequestSubmitted { request }).unwrap(),
            json!({
                "type": "request_submitted",
                "request": {
                    "request_id": "request-1",
                    "kind": {
                        "kind": "local_port_forward",
                        "remote_host": "127.0.0.1",
                        "remote_port": 8080,
                        "local_port": 18080
                    }
                }
            })
        );
    }

    #[test]
    fn file_copy_request_does_not_carry_an_authoritative_local_path() {
        let request = ServiceRequest {
            request_id: "copy-1".to_string(),
            kind: ServiceRequestKind::FileCopy {
                direction: FileCopyDirection::Download,
                remote_path: "/home/beams/report.txt".to_string(),
                local_path_hint: Some("report.txt".to_string()),
                overwrite: OverwritePolicy::Deny,
            },
        };
        let value = serde_json::to_value(BeamToClient::RequestSubmitted { request }).unwrap();
        assert_eq!(value["request"]["kind"]["kind"], "file_copy");
        assert_eq!(value["request"]["kind"]["direction"], "download");
        assert_eq!(value["request"]["kind"]["local_path_hint"], "report.txt");
        assert!(value["request"]["kind"].get("local_path").is_none());
    }

    #[test]
    fn folder_sync_request_does_not_carry_an_authoritative_local_path() {
        let request = ServiceRequest {
            request_id: "sync-1".to_string(),
            kind: ServiceRequestKind::FolderSync {
                remote_path: "/home/beams/project".to_string(),
                local_path_hint: Some("project".to_string()),
                mode: FolderSyncMode::TwoWaySafe,
                ignore_vcs: true,
                ignores: vec!["target".to_string()],
            },
        };
        let value = serde_json::to_value(BeamToClient::RequestSubmitted { request }).unwrap();
        assert_eq!(value["request"]["kind"]["kind"], "folder_sync");
        assert_eq!(value["request"]["kind"]["mode"], "two-way-safe");
        assert_eq!(value["request"]["kind"]["local_path_hint"], "project");
        assert!(value["request"]["kind"].get("local_path").is_none());
    }

    #[test]
    fn only_the_client_direction_contains_request_decisions() {
        let update = ClientToBeam::RequestStatus {
            request_id: "request-1".to_string(),
            status: RequestStatus::Rejected,
            message: Some("rejected locally".to_string()),
        };
        let value = serde_json::to_value(update).unwrap();
        assert_eq!(value["type"], "request_status");
        assert_eq!(value["status"], "rejected");

        assert!(serde_json::from_value::<BeamToClient>(value).is_err());
    }

    #[test]
    fn transition_tables_reject_backwards_or_implicit_changes() {
        assert_eq!(
            RequestStatus::PendingApproval.transition(RequestStatus::Approved),
            Ok(RequestStatus::Approved)
        );
        assert!(RequestStatus::Rejected
            .transition(RequestStatus::Running)
            .is_err());
        assert_eq!(
            ServiceStatus::Running.transition(ServiceStatus::Stopping),
            Ok(ServiceStatus::Stopping)
        );
        assert!(ServiceStatus::Stopped
            .transition(ServiceStatus::Running)
            .is_err());
        assert_eq!(
            ServiceStatus::Stopped.transition(ServiceStatus::Starting),
            Ok(ServiceStatus::Starting)
        );
        assert_eq!(
            ServiceStatus::Running.transition(ServiceStatus::Stopped),
            Ok(ServiceStatus::Stopped)
        );
        assert_eq!(
            ServiceStatus::Starting.transition(ServiceStatus::Stopping),
            Ok(ServiceStatus::Stopping)
        );
        assert_eq!(
            ServiceStatus::Running.transition(ServiceStatus::Paused),
            Ok(ServiceStatus::Paused)
        );
        assert_eq!(
            ServiceStatus::Paused.transition(ServiceStatus::Running),
            Ok(ServiceStatus::Running)
        );
        assert_eq!(
            ServiceStatus::Paused.transition(ServiceStatus::Degraded),
            Ok(ServiceStatus::Degraded)
        );
        assert_eq!(
            ServiceStatus::Starting.transition(ServiceStatus::Paused),
            Ok(ServiceStatus::Paused)
        );
        assert_eq!(
            ServiceStatus::Starting.transition(ServiceStatus::Degraded),
            Ok(ServiceStatus::Degraded)
        );
    }

    #[test]
    fn remote_service_actions_are_proposals_not_commands() {
        let request = ServiceActionRequest {
            request_id: "action-1".to_string(),
            service_id: "service-1".to_string(),
            action: ServiceAction::Stop,
        };
        let value = serde_json::to_value(BeamToClient::ServiceActionRequested { request }).unwrap();
        assert_eq!(value["type"], "service_action_requested");
        assert_eq!(value["request"]["action"], "stop");
        assert!(serde_json::from_value::<ClientToBeam>(value).is_err());
    }

    #[test]
    fn known_host_names_are_plain_names_or_bracketed_ports() {
        for ok in ["github.com", "10.0.0.1", "[git.example.com]:2222", "fe80::1", "host_1"] {
            assert!(valid_known_host_name(ok), "{ok}");
        }
        for bad in ["", "-oProxyCommand=x", "*.example.com", "a,b", "!host", "[host]", "[host]:", "[host]:x", "a b", "h\n"] {
            assert!(!valid_known_host_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn file_fetch_request_has_a_stable_tagged_shape() {
        let request = ServiceRequest {
            request_id: "request-9".to_string(),
            kind: ServiceRequestKind::FileFetch {
                sources: vec![FetchSource {
                    path: "/home/beams/out.log".to_string(),
                    size: 42,
                    is_dir: false,
                }],
                local_path_hint: None,
                overwrite: OverwritePolicy::Deny,
            },
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "request_id": "request-9",
                "kind": {
                    "kind": "file_fetch",
                    "sources": [{"path": "/home/beams/out.log", "size": 42, "is_dir": false}],
                    "overwrite": "deny",
                },
            })
        );
    }

    #[test]
    fn hello_capabilities_are_optional_both_ways() {
        let hello = serde_json::to_value(ClientToBeam::hello("c", "b", Vec::new())).unwrap();
        assert_eq!(hello["capabilities"], json!(["file_fetch", "ssh_agent", "launch", "known_hosts"]));
        // A hello from a workstation that predates capabilities.
        let old: ClientToBeam = serde_json::from_value(json!({
            "type": "hello",
            "protocol": PROTOCOL,
            "version": PROTOCOL_VERSION,
            "client_id": "c",
            "bridge_id": "b",
            "session_tokens": [],
        }))
        .unwrap();
        assert!(matches!(old, ClientToBeam::Hello { capabilities, .. } if capabilities.is_empty()));
    }
}
