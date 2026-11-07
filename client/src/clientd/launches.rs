//! Configured launches this workstation drives onto its Beams (`beams config
//! launch`; `docs/development/launches.md`, "Launches over the workstation
//! bridge").
//!
//! The daemon owns each launch from the moment a frontend starts it: the
//! record (Beam, local launch directory, gate, last known status) is saved
//! before anything is uploaded and survives restarts. Each time a Beam's
//! bridge connects, every unfinished launch for it is driven again from
//! the top: begin the declaration upload (the Beam answers at once if it
//! already has it, or has accepted the launch), submit (idempotent), then
//! upload pre unless it was released, and post once `interface-ready` is.
//! Uploads resume from the bytes the Beam reports. Everything runs on the
//! bridge's event thread except packing post, which may hash a large tree.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use beams_protocol::launch::{
    Blob, BundleDescriptor, Compression, Declaration, Gate, LaunchStatus, UPLOAD_CHUNK_BYTES, UPLOAD_WINDOW,
};
use beams_protocol::{BeamToClient, ClientToBeam, CAPABILITY_LAUNCH};
use serde::{Deserialize, Serialize};

use super::{Hub, ServerEvent};

/// Upload failures in a row before a launch gives up (until the next
/// connection, or a new `start_launch`).
const MAX_UPLOAD_FAILURES: u32 = 3;
/// How often upload progress is broadcast to frontends.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchStage {
    /// Not yet accepted by the Beam.
    Submitting,
    Accepted,
    /// Finished on the Beam (or rejected by it); nothing more to drive.
    Finished,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UploadProgress {
    pub blob: Blob,
    pub sent: u64,
    pub total: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LaunchRecord {
    pub launch_id: String,
    pub plan_digest: String,
    pub beam_id: String,
    pub beam_uuid: String,
    pub login: String,
    /// The `beams config pack` output: `launch.json`, `pre/`, later `post/`.
    pub launch_dir: PathBuf,
    pub gate: Gate,
    pub stage: LaunchStage,
    /// The Beam's last reported status.
    #[serde(default)]
    pub status: Option<LaunchStatus>,
    /// Conditions the Beam reported released.
    #[serde(default)]
    pub released: Vec<String>,
    /// The launch's `[ssh_grant]` was issued (at most once per launch, so a
    /// forced reconnect can't refill its budget).
    #[serde(default)]
    pub grant_issued: bool,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub upload: Option<UploadProgress>,
}

/// One upload in flight for a launch.
struct Upload {
    blob: Blob,
    path: PathBuf,
    bytes: u64,
    /// Acknowledged by the Beam.
    acked: u64,
    /// Sent so far (at most a window ahead of `acked`).
    sent: u64,
    /// The end offset of each chunk sent but not yet acknowledged, oldest
    /// first: the acknowledgements expected, in order.
    in_flight: VecDeque<u64>,
    last_progress: Instant,
}

impl Upload {
    /// Applies the Beam's report that it holds `offset` bytes: the next
    /// expected acknowledgement, or anything else (the reply to a begin, a
    /// refused chunk), after which sending resumes from `offset`.
    fn held(&mut self, offset: u64) {
        let offset = offset.min(self.bytes);
        if self.in_flight.front() == Some(&offset) {
            self.in_flight.pop_front();
        } else if offset == self.acked && !self.in_flight.is_empty() {
            // A refusal of a chunk sent before the last resync repeats
            // what is already known; resending again would only multiply
            // them. (Chunks are lost only across reconnects, which begin
            // the upload afresh.)
            return;
        } else {
            self.in_flight.clear();
            self.sent = offset;
        }
        self.acked = offset;
    }

    /// The next chunk to send, if the window and the blob allow one.
    fn next_chunk(&self) -> Option<(u64, usize)> {
        let window = (UPLOAD_WINDOW * UPLOAD_CHUNK_BYTES) as u64;
        if self.sent >= self.bytes || self.sent.saturating_sub(self.acked) >= window {
            return None;
        }
        Some((self.sent, (self.bytes - self.sent).min(UPLOAD_CHUNK_BYTES as u64) as usize))
    }

    fn sent_chunk(&mut self, len: usize) {
        self.sent += len as u64;
        self.in_flight.push_back(self.sent);
    }
}

#[derive(Default)]
pub(super) struct Runtime {
    /// Beams whose bridge advertised `CAPABILITY_LAUNCH` on this connection.
    capable: BTreeSet<String>,
    uploads: BTreeMap<String, Upload>,
    failures: BTreeMap<String, u32>,
    /// Launches whose post bundle is being packed.
    packing: BTreeSet<String>,
}

/// Where a blob lives in a launch directory, and what its upload begins
/// with.
fn blob_source(launch_dir: &Path, blob: Blob) -> anyhow::Result<(PathBuf, u64, String, Option<BundleDescriptor>)> {
    match blob.phase() {
        None => {
            let path = launch_dir.join("launch.json");
            let (sha256, bytes) = beams_artifacts::hash_file(&path)?;
            Ok((path, bytes, sha256, None))
        }
        Some(phase) => {
            let dir = launch_dir.join(phase.name());
            let descriptor: BundleDescriptor = beams_artifacts::read_json(&dir.join("descriptor.json"))?;
            let name = match descriptor.compression {
                Compression::None => "bundle.tar",
                Compression::Gzip => "bundle.tar.gz",
            };
            Ok((dir.join(name), descriptor.bytes, descriptor.sha256.clone(), Some(descriptor)))
        }
    }
}

impl Hub {
    /// Starts driving the launch packed in `launch_dir` onto `target`
    /// (idempotent: the same launch again just drives it again).
    pub(super) fn start_launch(
        self: &Arc<Self>,
        target: &crate::bridge::Target,
        launch_dir: &Path,
        gate: Gate,
    ) -> anyhow::Result<LaunchRecord> {
        let launch_dir = std::path::absolute(launch_dir)?;
        let declaration: Declaration = beams_artifacts::read_json(&launch_dir.join("launch.json"))?;
        blob_source(&launch_dir, Blob::Pre).context("the launch directory has no packed pre bundle")?;
        let record = {
            let mut state = self.state.lock().unwrap();
            let existing = state.launches.get(&declaration.launch_id).cloned();
            match existing {
                Some(existing) if existing.beam_uuid != target.beam_uuid => {
                    anyhow::bail!("launch {} belongs to Beam {}", existing.launch_id, existing.beam_id)
                }
                Some(existing) => existing,
                None => {
                    let record = LaunchRecord {
                        launch_id: declaration.launch_id.clone(),
                        plan_digest: declaration.plan_digest.clone(),
                        beam_id: target.beam_id.clone(),
                        beam_uuid: target.beam_uuid.clone(),
                        login: target.login.clone(),
                        launch_dir,
                        gate,
                        stage: LaunchStage::Submitting,
                        status: None,
                        released: Vec::new(),
                        grant_issued: false,
                        message: Some("waiting for the Beam bridge".into()),
                        upload: None,
                    };
                    state.launches.insert(record.launch_id.clone(), record.clone());
                    record
                }
            }
        };
        self.save_state()?;
        self.broadcast(ServerEvent::LaunchChanged { launch: record.clone() });
        self.launch_runtime.lock().unwrap().failures.remove(&record.launch_id);
        if self.launch_runtime.lock().unwrap().capable.contains(&record.beam_uuid) {
            self.drive_launch(&record.launch_id);
        }
        Ok(record)
    }

    /// The Beam's bridge (re)connected: drive its unfinished launches.
    pub(super) fn launch_bridge_connected(self: &Arc<Self>, beam_uuid: &str, capabilities: &[String]) {
        {
            let mut runtime = self.launch_runtime.lock().unwrap();
            if !capabilities.iter().any(|c| c == CAPABILITY_LAUNCH) {
                runtime.capable.remove(beam_uuid);
                drop(runtime);
                self.report_incapable(beam_uuid);
                return;
            }
            runtime.capable.insert(beam_uuid.to_string());
        }
        let ids: Vec<String> = self
            .state
            .lock()
            .unwrap()
            .launches
            .values()
            .filter(|l| l.beam_uuid == beam_uuid && l.stage != LaunchStage::Finished)
            .map(|l| l.launch_id.clone())
            .collect();
        for id in ids {
            self.launch_runtime.lock().unwrap().failures.remove(&id);
            self.drive_launch(&id);
        }
    }

    /// The bridge connected without `CAPABILITY_LAUNCH`: its `beam-bootstrap`
    /// predates launches. Say so instead of "waiting for the Beam bridge".
    fn report_incapable(&self, beam_uuid: &str) {
        let ids: Vec<String> = self
            .state
            .lock()
            .unwrap()
            .launches
            .values()
            .filter(|l| l.beam_uuid == beam_uuid && l.stage != LaunchStage::Finished)
            .map(|l| l.launch_id.clone())
            .collect();
        for id in ids {
            self.update_launch(&id, |l| {
                l.message = Some(
                    "the Beam's beam-bootstrap has no launch support (too old); update its runtime or use a new Beam"
                        .into(),
                )
            });
        }
    }

    /// The bridge went away: in-flight uploads resume on reconnection.
    pub(super) fn launch_bridge_disconnected(&self, beam_uuid: &str) {
        let mut runtime = self.launch_runtime.lock().unwrap();
        runtime.capable.remove(beam_uuid);
        let launches = self.state.lock().unwrap().launches.clone();
        runtime.uploads.retain(|id, _| launches.get(id).is_none_or(|l| l.beam_uuid != beam_uuid));
    }

    fn drive_launch(self: &Arc<Self>, id: &str) {
        self.begin_upload(id, Blob::Declaration);
    }

    /// Issues launch `id`'s `[ssh_grant]` (from its workstation-side
    /// `ssh-grant.json`) once the covered tasks' phase can start: at
    /// acceptance for pre, at `interface-ready` for post. At most once.
    fn issue_launch_grant(self: &Arc<Self>, id: &str) {
        let Some(record) = self.launch_record(id) else { return };
        if record.grant_issued || record.stage == LaunchStage::Finished {
            return;
        }
        let path = record.launch_dir.join(crate::startup::LaunchGrant::FILE);
        if !path.exists() {
            return;
        }
        let grant: crate::startup::LaunchGrant = match beams_artifacts::read_json(&path) {
            Ok(grant) => grant,
            Err(err) => {
                eprintln!("launch {id}: reading {}: {err:#}", path.display());
                return;
            }
        };
        let open = grant.phase == beams_protocol::launch::Phase::Pre
            || record.released.iter().any(|c| c == "interface-ready");
        if !open {
            return;
        }
        self.update_launch(id, |l| l.grant_issued = true);
        self.issue_grant(
            &record.beam_uuid,
            id,
            grant.hosts.clone(),
            grant.user.clone(),
            std::time::Duration::from_secs(grant.window_secs),
            grant.max_uses,
            Some(grant.task_runs(id).into_iter().collect()),
        );
    }

    fn launch_record(&self, id: &str) -> Option<LaunchRecord> {
        self.state.lock().unwrap().launches.get(id).cloned()
    }

    /// Applies `change` to launch `id`, saves and broadcasts it.
    fn update_launch(&self, id: &str, change: impl FnOnce(&mut LaunchRecord)) -> Option<LaunchRecord> {
        let record = {
            let mut state = self.state.lock().unwrap();
            let record = state.launches.get_mut(id)?;
            change(record);
            record.clone()
        };
        if let Err(err) = self.save_state() {
            eprintln!("launch {id}: saving state: {err:#}");
        }
        self.broadcast(ServerEvent::LaunchChanged { launch: record.clone() });
        Some(record)
    }

    fn send_to_beam(&self, beam_uuid: &str, message: ClientToBeam) -> bool {
        let commands = self.bridges.lock().unwrap().get(beam_uuid).map(|b| b.commands.clone());
        // Blocking is fine: the bridge writer drains the queue as it fills,
        // and an upload never has more than a window queued.
        commands.is_some_and(|commands| commands.send(message).is_ok())
    }

    fn begin_upload(self: &Arc<Self>, id: &str, blob: Blob) {
        let Some(record) = self.launch_record(id) else { return };
        let (path, bytes, sha256, descriptor) = match blob_source(&record.launch_dir, blob) {
            Ok(source) => source,
            Err(err) => {
                self.update_launch(id, |l| l.message = Some(format!("reading the {} upload: {err:#}", blob.name())));
                return;
            }
        };
        self.launch_runtime.lock().unwrap().uploads.insert(
            id.to_string(),
            Upload { blob, path, bytes, acked: 0, sent: 0, in_flight: VecDeque::new(), last_progress: Instant::now() },
        );
        if blob != Blob::Declaration {
            self.update_launch(id, |l| {
                l.upload = Some(UploadProgress { blob, sent: 0, total: bytes });
                l.message = Some(format!("uploading the {} bundle", blob.name()));
            });
        }
        self.send_to_beam(
            &record.beam_uuid,
            ClientToBeam::LaunchUploadBegin { launch_id: id.to_string(), blob, bytes, sha256, descriptor },
        );
    }

    /// Sends chunks until the window is full or the blob is all sent.
    fn pump_upload(self: &Arc<Self>, id: &str, beam_uuid: &str) {
        loop {
            let chunk = {
                let mut runtime = self.launch_runtime.lock().unwrap();
                let Some(upload) = runtime.uploads.get_mut(id) else { return };
                let Some((offset, len)) = upload.next_chunk() else { return };
                let mut data = vec![0; len];
                let read = File::open(&upload.path)
                    .and_then(|mut file| {
                        file.seek(SeekFrom::Start(offset))?;
                        file.read_exact(&mut data)
                    })
                    .with_context(|| format!("reading {}", upload.path.display()));
                match read {
                    Ok(()) => {
                        let chunk = ClientToBeam::LaunchUploadChunk {
                            launch_id: id.to_string(),
                            blob: upload.blob,
                            offset,
                            data: beams_protocol::agent::b64_encode(&data),
                        };
                        upload.sent_chunk(len);
                        Ok(chunk)
                    }
                    Err(err) => {
                        runtime.uploads.remove(id);
                        Err(err)
                    }
                }
            };
            match chunk {
                Ok(chunk) => {
                    if !self.send_to_beam(beam_uuid, chunk) {
                        return;
                    }
                }
                Err(err) => {
                    self.update_launch(id, |l| {
                        l.upload = None;
                        l.message = Some(format!("{err:#}"));
                    });
                    return;
                }
            }
        }
    }

    fn start_post(self: &Arc<Self>, id: &str) {
        let Some(record) = self.launch_record(id) else { return };
        if record.released.iter().any(|c| c == "artifact-ready:post")
            || !self.launch_runtime.lock().unwrap().packing.insert(id.to_string())
        {
            return;
        }
        self.update_launch(id, |l| l.message = Some("packing the post bundle".into()));
        let hub = Arc::clone(self);
        let id = id.to_string();
        thread::spawn(move || {
            let packed = crate::config::pack::prepare_post(&record.launch_dir);
            hub.launch_runtime.lock().unwrap().packing.remove(&id);
            match packed {
                Ok(_) => hub.begin_upload(&id, Blob::Post),
                Err(err) => {
                    hub.update_launch(&id, |l| l.message = Some(format!("packing the post bundle: {err:#}")));
                }
            }
        });
    }

    /// What to do next once the Beam has accepted the launch.
    fn continue_launch(self: &Arc<Self>, id: &str) {
        let Some(record) = self.launch_record(id) else { return };
        if record.stage == LaunchStage::Finished {
            return;
        }
        let uploading = self.launch_runtime.lock().unwrap().uploads.contains_key(id);
        if uploading {
            return;
        }
        if !record.released.iter().any(|c| c == "artifact-ready:pre") {
            self.begin_upload(id, Blob::Pre);
        } else if record.released.iter().any(|c| c == "interface-ready") {
            self.start_post(id);
        }
    }

    /// Handles a launch reply from the Beam `beam_uuid`.
    pub(super) fn launch_message(self: &Arc<Self>, beam_uuid: &str, message: BeamToClient) {
        let owned = |id: &str| self.launch_record(id).is_some_and(|l| l.beam_uuid == beam_uuid);
        match message {
            BeamToClient::LaunchUploadOffset { launch_id, blob, offset } if owned(&launch_id) => {
                let progress = {
                    let mut runtime = self.launch_runtime.lock().unwrap();
                    let Some(upload) = runtime.uploads.get_mut(&launch_id).filter(|u| u.blob == blob) else {
                        return;
                    };
                    upload.held(offset);
                    let due = upload.last_progress.elapsed() >= PROGRESS_INTERVAL;
                    if due {
                        upload.last_progress = Instant::now();
                    }
                    (due && blob != Blob::Declaration).then(|| UploadProgress { blob, sent: upload.acked, total: upload.bytes })
                };
                if let Some(progress) = progress {
                    self.update_launch(&launch_id, |l| l.upload = Some(progress));
                }
                self.pump_upload(&launch_id, beam_uuid);
            }
            BeamToClient::LaunchUploaded { launch_id, blob } if owned(&launch_id) => {
                {
                    let mut runtime = self.launch_runtime.lock().unwrap();
                    if runtime.uploads.get(&launch_id).is_some_and(|u| u.blob == blob) {
                        runtime.uploads.remove(&launch_id);
                    }
                    runtime.failures.remove(&launch_id);
                }
                match blob {
                    Blob::Declaration => {
                        let Some(record) = self.launch_record(&launch_id) else { return };
                        self.send_to_beam(
                            beam_uuid,
                            ClientToBeam::LaunchSubmit {
                                request_id: uuid::Uuid::new_v4().to_string(),
                                launch_id: launch_id.clone(),
                                plan_digest: record.plan_digest,
                                gate: record.gate,
                            },
                        );
                    }
                    Blob::Pre | Blob::Post => {
                        self.update_launch(&launch_id, |l| {
                            l.upload = None;
                            l.message = Some(format!("{} bundle uploaded; the Beam is staging it", blob.name()));
                        });
                    }
                }
            }
            BeamToClient::LaunchUploadFailed { launch_id, blob, message } if owned(&launch_id) => {
                let failures = {
                    let mut runtime = self.launch_runtime.lock().unwrap();
                    runtime.uploads.remove(&launch_id);
                    let failures = runtime.failures.entry(launch_id.clone()).or_default();
                    *failures += 1;
                    *failures
                };
                let gave_up = failures >= MAX_UPLOAD_FAILURES;
                self.update_launch(&launch_id, |l| {
                    l.upload = None;
                    l.message = Some(if gave_up {
                        format!("the {} upload failed {failures} times: {message}", blob.name())
                    } else {
                        format!("retrying the {} upload: {message}", blob.name())
                    });
                });
                if !gave_up {
                    self.begin_upload(&launch_id, blob);
                }
            }
            BeamToClient::LaunchAccepted { launch_id, existing, status, released, .. } if owned(&launch_id) => {
                let finished = status.finished();
                self.update_launch(&launch_id, |l| {
                    l.stage = if finished { LaunchStage::Finished } else { LaunchStage::Accepted };
                    l.status = Some(status);
                    l.released = released;
                    l.message = Some(if existing { "the Beam already has this launch" } else { "accepted by the Beam" }.into());
                });
                if finished {
                    self.end_grants_for_scope(&launch_id, "the launch finished");
                } else {
                    self.issue_launch_grant(&launch_id);
                }
                self.continue_launch(&launch_id);
            }
            BeamToClient::LaunchRejected { launch_id, message, .. } if owned(&launch_id) => {
                self.update_launch(&launch_id, |l| {
                    l.stage = LaunchStage::Finished;
                    l.status = Some(LaunchStatus::Failed);
                    l.message = Some(format!("rejected by the Beam: {message}"));
                });
            }
            BeamToClient::LaunchStatus { launch_id, status, message } if owned(&launch_id) => {
                if status.finished() {
                    self.end_grants_for_scope(&launch_id, "the launch finished");
                }
                self.update_launch(&launch_id, |l| {
                    if status.finished() {
                        l.stage = LaunchStage::Finished;
                        l.upload = None;
                    }
                    l.status = Some(status);
                    if message.is_some() || status.finished() {
                        l.message = message;
                    }
                });
            }
            BeamToClient::LaunchCondition { launch_id, condition, released, message } if owned(&launch_id) => {
                self.update_launch(&launch_id, |l| {
                    if released {
                        if !l.released.contains(&condition) {
                            l.released.push(condition.clone());
                        }
                        l.message = Some(format!("released {condition}"));
                    } else {
                        l.message = Some(format!("{condition} failed: {}", message.unwrap_or_default()));
                    }
                });
                if released && condition == "interface-ready" {
                    self.issue_launch_grant(&launch_id);
                    self.continue_launch(&launch_id);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload(bytes: u64) -> Upload {
        Upload {
            blob: Blob::Post,
            path: PathBuf::new(),
            bytes,
            acked: 0,
            sent: 0,
            in_flight: VecDeque::new(),
            last_progress: Instant::now(),
        }
    }

    /// Sends what the window allows; returns the chunk offsets.
    fn fill(upload: &mut Upload) -> Vec<u64> {
        let mut sent = Vec::new();
        while let Some((offset, len)) = upload.next_chunk() {
            sent.push(offset);
            upload.sent_chunk(len);
        }
        sent
    }

    #[test]
    fn the_window_bounds_unacknowledged_chunks_and_acks_slide_it() {
        let chunk = UPLOAD_CHUNK_BYTES as u64;
        let mut u = upload(20 * chunk + 5);
        u.held(0); // the begin reply
        assert_eq!(fill(&mut u).len(), UPLOAD_WINDOW);
        // Pipelined acks are expected, not a reason to resend.
        u.held(chunk);
        u.held(2 * chunk);
        assert_eq!(fill(&mut u), [8 * chunk, 9 * chunk]);
        assert_eq!(u.sent, 10 * chunk);
        // Run to the end; the last chunk is short.
        let mut acked = 2 * chunk;
        while u.acked < u.bytes {
            acked = (acked + chunk).min(u.bytes);
            u.held(acked);
            fill(&mut u);
        }
        assert_eq!(u.sent, u.bytes);
        assert!(u.next_chunk().is_none());
    }

    #[test]
    fn an_unexpected_offset_resumes_from_what_the_beam_holds() {
        let chunk = UPLOAD_CHUNK_BYTES as u64;
        let mut u = upload(20 * chunk);
        // Resuming: the begin reply says the Beam already holds 3 chunks.
        u.held(3 * chunk);
        let first = fill(&mut u);
        assert_eq!((first[0], first.len()), (3 * chunk, UPLOAD_WINDOW));
        // The expected ack slides the window by one.
        u.held(4 * chunk);
        assert_eq!(fill(&mut u), [11 * chunk]);
        // The Beam refused a chunk and holds less: resend from there.
        u.held(2 * chunk);
        assert_eq!(u.sent, 2 * chunk, "a lower count rewinds");
        assert_eq!(fill(&mut u)[0], 2 * chunk);
        // Refusals of chunks sent before that repeat it, and change nothing.
        let sent = u.sent;
        u.held(2 * chunk);
        assert_eq!(u.sent, sent);
        assert!(fill(&mut u).is_empty());
        // A count beyond what was sent (a stale reply) never underflows.
        u.held(19 * chunk);
        assert_eq!(fill(&mut u), [19 * chunk]);
    }
}
