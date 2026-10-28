//! Typed messages shared by the workstation `beams` daemon and the
//! in-VM `beam-bootstrap client-bridge`.
//!
//! The Beam side is allowed to submit proposals and observe status. It
//! cannot encode an approval: request decisions only exist in
//! [`ClientToBeam`], whose sender is the workstation daemon.

use serde::{Deserialize, Serialize};

pub const PROTOCOL: &str = "beams-client-services";
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

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
            Self::FolderSync { .. } => "folder_sync",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ServiceProjection {
    pub service_id: String,
    pub request_id: String,
    pub kind: ServiceRequestKind,
    pub status: ServiceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
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
    },
    RequestSubmitted {
        request: ServiceRequest,
    },
    ServiceActionRequested {
        request: ServiceActionRequest,
    },
    Pong {
        nonce: String,
    },
    Error {
        message: String,
    },
}

impl BeamToClient {
    pub fn hello(bridge_id: impl Into<String>) -> Self {
        Self::Hello {
            protocol: PROTOCOL.to_string(),
            version: PROTOCOL_VERSION,
            bridge_id: bridge_id.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
