//! Tier B: one OpenSSH ControlMaster per Beam, owned by `beamsd`, with every
//! operation a channel on it (`docs/designs/BEAM_TRANSPORT_DESIGN.md`).
//!
//! The master's transport is the `tsh proxy ssh --proxy=P --cluster=C`
//! ProxyCommand from a private `tsh config --proxy=P`, so it's pinned to the
//! Beam's endpoint like everything else, and OpenSSH authenticates with the
//! profile's own key and certificate and checks the host against
//! Teleport's host CA. Profiles OpenSSH can't use (hardware keys), failed
//! authentication and missing tools fall back to [`TshConnection`] (Tier C)
//! per operation, so callers never see the difference beyond speed.
//!
//! Layout under the runtime directory (mode 0700):
//!
//! - `cfg/<endpoint hash>.conf`: the generated OpenSSH config.
//! - `m/<beam hash>.<generation>`: a master's control socket. A new
//!   generation replaces a master before its certificate expires; the old
//!   one keeps its own channels and forwards (and can still `-O cancel`
//!   them) until it's killed at expiry.
//! - `m/<beam hash>`: symlink to the current generation, for Mutagen's
//!   wrapper scripts, which run outside the daemon.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context};

use crate::connection::{BeamConnection, BeamTarget, ForwardPlan, TshConnection};
use crate::endpoint::{Endpoint, Tsh};

/// How long a master may take to authenticate (ProxyCommand start, proxy
/// dial, node handshake). From a workstation this is normally 1-2 s.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// Replace a master this long before its certificate expires.
const ROTATE_BEFORE_EXPIRY: Duration = Duration::from_secs(5 * 60);
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// A `tsh config` that failed for a transient reason (offline) is retried
/// after this long even if the profile hasn't changed.
const PROBE_RETRY: Duration = Duration::from_secs(60);
const RENEWAL_CHECK: Duration = Duration::from_secs(30);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);
const DIAGNOSTIC_LINES: usize = 20;
/// macOS `sun_path` is 104 bytes, and OpenSSH binds a 17-byte-longer
/// temporary name first.
const MAX_SOCKET_PATH: usize = 80;
/// Placeholder destination for `-O` control commands, which only use the
/// socket.
const CONTROL_HOST: &str = "beams-master";

/// Set `BEAMS_MULTIPLEX=0` to disable Tier B entirely.
pub fn enabled_by_env() -> bool {
    !matches!(std::env::var("BEAMS_MULTIPLEX").as_deref(), Ok("0" | "false" | "no" | "off"))
}

/// Shared handle on one master generation, held by forwards registered on
/// it. `alive` turns false once that master is gone, which removes its
/// forwards with it.
#[derive(Clone, Debug)]
pub struct Lease {
    pub socket: PathBuf,
    pub host: String,
    pub alive: Arc<AtomicBool>,
}

impl Lease {
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

pub struct Pool {
    root: PathBuf,
    sockets: PathBuf,
    tsh_program: String,
    ssh_program: String,
    scp_program: String,
    probes: Mutex<HashMap<Endpoint, Probe>>,
    slots: Mutex<HashMap<String, Arc<Mutex<Slot>>>>,
    draining: Mutex<Vec<Live>>,
    generation: AtomicU64,
    /// `ROTATE_BEFORE_EXPIRY`; tests shorten it.
    rotate_before: Mutex<Duration>,
    spawner: Mutex<Sender<SpawnRequest>>,
    stopped: AtomicBool,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool").field("root", &self.root).finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct Probe {
    stamp: Option<SystemTime>,
    checked: Instant,
    outcome: Result<Profile, Unusable>,
}

#[derive(Clone, Debug)]
struct Profile {
    config: PathBuf,
    certificate: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct Unusable {
    reason: String,
    /// Retried after `PROBE_RETRY` even without a new login.
    transient: bool,
}

enum Slot {
    Empty,
    Ready(Live),
    Failed { until: Instant, delay: Duration, reason: String },
}

struct Live {
    child: Child,
    lease: Lease,
    /// Start a replacement from this time (certificate expiry minus margin).
    rotate_at: Option<SystemTime>,
    /// The certificate's expiry: killed then, whether current or draining.
    expires_at: Option<SystemTime>,
    /// When to next look for a renewed certificate once `rotate_at` passed.
    next_renewal_check: Instant,
}

impl Live {
    /// Whether the profile now has a certificate that outlives this
    /// master's. Checked at most every `RENEWAL_CHECK`; until then (or
    /// without a new login) the master stays in use until it expires.
    fn renewed(&mut self, profile: &Profile) -> bool {
        if Instant::now() < self.next_renewal_check {
            return false;
        }
        self.next_renewal_check = Instant::now() + RENEWAL_CHECK;
        let current = profile.certificate.as_deref().and_then(certificate_expiry);
        matches!((current, self.expires_at), (Some(current), Some(ours)) if current > ours)
    }
}

type SpawnRequest = (Command, Sender<std::io::Result<Child>>);

impl Pool {
    /// A pool keeping its sockets and configs under `root`, which is created
    /// mode 0700. Masters left behind by an earlier daemon are closed.
    pub fn new(root: PathBuf, tsh_program: String) -> anyhow::Result<Arc<Self>> {
        let ssh_program = std::env::var("BEAMS_SSH").unwrap_or_else(|_| "ssh".to_string());
        let scp_program = std::env::var("BEAMS_SCP").unwrap_or_else(|_| "scp".to_string());
        Self::with_programs(root, tsh_program, ssh_program, scp_program)
    }

    pub fn with_programs(
        root: PathBuf,
        tsh_program: String,
        ssh_program: String,
        scp_program: String,
    ) -> anyhow::Result<Arc<Self>> {
        let root = private_dir(&root)?;
        let mut sockets = private_dir(&root.join("m"))?;
        if sockets.as_os_str().len() + 20 > MAX_SOCKET_PATH {
            sockets = private_dir(&std::env::temp_dir().join(format!("beams-ssh-{}", unsafe { libc::getuid() })))?;
        }
        private_dir(&root.join("cfg"))?;

        let (spawn_tx, spawn_rx) = mpsc::channel::<SpawnRequest>();
        // Masters are spawned from this long-lived thread: Linux's
        // parent-death signal fires when the spawning *thread* exits, and a
        // master must outlive the worker that first needed it.
        thread::Builder::new()
            .name("beams-ssh-spawner".into())
            .spawn(move || {
                for (mut command, reply) in spawn_rx {
                    let _ = reply.send(command.spawn());
                }
            })
            .context("starting the SSH master spawner")?;

        let pool = Arc::new(Self {
            root,
            sockets,
            tsh_program,
            ssh_program,
            scp_program,
            probes: Mutex::new(HashMap::new()),
            slots: Mutex::new(HashMap::new()),
            draining: Mutex::new(Vec::new()),
            generation: AtomicU64::new(1),
            rotate_before: Mutex::new(ROTATE_BEFORE_EXPIRY),
            spawner: Mutex::new(spawn_tx),
            stopped: AtomicBool::new(false),
        });
        pool.close_stale_masters();
        let weak = Arc::downgrade(&pool);
        thread::Builder::new()
            .name("beams-ssh-masters".into())
            .spawn(move || maintain(weak))
            .context("starting SSH master maintenance")?;
        Ok(pool)
    }

    /// The connection workers use for `target`: Tier B while a master can be
    /// had, Tier C per operation otherwise.
    pub fn connection(self: &Arc<Self>, tsh_program: &str, target: BeamTarget) -> Arc<dyn BeamConnection> {
        Arc::new(MultiplexedConnection {
            fallback: TshConnection::new(tsh_program, target.clone()),
            target,
            pool: Arc::clone(self),
        })
    }

    /// A healthy master for `target`, starting one if needed. Blocks while
    /// another caller is starting it. `Err` is why Tier C is used instead.
    pub fn lease(&self, target: &BeamTarget) -> Result<Lease, String> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err("the daemon is stopping".into());
        }
        let endpoint = target.endpoint.as_ref().ok_or("the Beam has no recorded Teleport endpoint")?;
        let profile = self.profile(endpoint).map_err(|unusable| unusable.reason)?;
        let slot = Arc::clone(self.slots.lock().unwrap().entry(target.beam_uuid.clone()).or_insert_with(|| Arc::new(Mutex::new(Slot::Empty))));
        let mut slot = slot.lock().unwrap();
        let now = SystemTime::now();
        match &mut *slot {
            Slot::Ready(live) => {
                let running = matches!(live.child.try_wait(), Ok(None));
                let expired = live.expires_at.is_some_and(|at| now >= at);
                if running && !expired {
                    if !live.rotate_at.is_some_and(|at| now >= at) || !live.renewed(&profile) {
                        return Ok(live.lease.clone());
                    }
                    // A newer certificate exists (`tsh login`): new channels
                    // go to a master using it; this one keeps what it has
                    // until its own certificate expires.
                    let Slot::Ready(old) = std::mem::replace(&mut *slot, Slot::Empty) else { unreachable!() };
                    eprintln!("beams daemon: moving {} to a new SSH master with the renewed certificate", target.beam_uuid);
                    self.draining.lock().unwrap().push(old);
                } else {
                    // Dead, or past its certificate's expiry: channels must
                    // not outlive the credential that authorized them.
                    let Slot::Ready(old) = std::mem::replace(&mut *slot, Slot::Empty) else { unreachable!() };
                    self.kill(old);
                }
            }
            Slot::Failed { until, reason, .. } if Instant::now() < *until => return Err(reason.clone()),
            _ => {}
        }
        let delay = match &*slot {
            Slot::Failed { delay, .. } => (*delay * 2).min(RETRY_MAX),
            _ => RETRY_MIN,
        };
        match self.start(target, endpoint, &profile) {
            Ok(live) => {
                let lease = live.lease.clone();
                eprintln!("beams daemon: multiplexed SSH connection to {} is up", target.beam_uuid);
                *slot = Slot::Ready(live);
                Ok(lease)
            }
            Err(Failure { reason, profile_unusable }) => {
                eprintln!("beams daemon: SSH master for {} unavailable, using per-operation tsh: {reason}", target.beam_uuid);
                if profile_unusable {
                    // Not worth retrying until the next `tsh login`.
                    if let Some(probe) = self.probes.lock().unwrap().get_mut(endpoint) {
                        probe.outcome = Err(Unusable { reason: reason.clone(), transient: false });
                    }
                }
                *slot = Slot::Failed { until: Instant::now() + delay, delay, reason: reason.clone() };
                Err(reason)
            }
        }
    }

    /// The current master's stable socket path for `beam_uuid` (a symlink),
    /// whether or not one is running.
    pub fn stable_socket(&self, beam_uuid: &str) -> PathBuf {
        self.sockets.join(short_hash(beam_uuid))
    }

    /// Whether `endpoint` can use Tier B at all (the probe only; doesn't
    /// start anything).
    pub fn supports(&self, endpoint: &Endpoint) -> bool {
        self.profile(endpoint).is_ok()
    }

    /// Closes `beam_uuid`'s master (its Beam is gone or no longer used).
    pub fn close(&self, beam_uuid: &str) {
        let slot = self.slots.lock().unwrap().remove(beam_uuid);
        if let Some(slot) = slot {
            if let Slot::Ready(live) = std::mem::replace(&mut *slot.lock().unwrap(), Slot::Empty) {
                self.kill(live);
            }
        }
        let _ = fs::remove_file(self.stable_socket(beam_uuid));
    }

    /// Closes every master. Further leases fall back to Tier C.
    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let slots: Vec<_> = self.slots.lock().unwrap().drain().map(|(_, slot)| slot).collect();
        for slot in slots {
            if let Slot::Ready(live) = std::mem::replace(&mut *slot.lock().unwrap(), Slot::Empty) {
                self.kill(live);
            }
        }
        for live in std::mem::take(&mut *self.draining.lock().unwrap()) {
            self.kill(live);
        }
    }

    fn kill(&self, mut live: Live) {
        live.lease.alive.store(false, Ordering::SeqCst);
        let _ = self.control(&live.lease.socket, "exit");
        if !matches!(live.child.try_wait(), Ok(Some(_))) {
            let _ = live.child.kill();
        }
        let _ = live.child.wait();
        let _ = fs::remove_file(&live.lease.socket);
    }

    fn profile(&self, endpoint: &Endpoint) -> Result<Profile, Unusable> {
        let stamp = profile_stamp(endpoint);
        let mut probes = self.probes.lock().unwrap();
        if let Some(probe) = probes.get(endpoint) {
            let fresh = probe.stamp == stamp
                && match &probe.outcome {
                    Err(unusable) if unusable.transient => probe.checked.elapsed() < PROBE_RETRY,
                    _ => true,
                };
            if fresh {
                return probe.outcome.clone();
            }
        }
        let outcome = self.probe(endpoint);
        match &outcome {
            Ok(_) => eprintln!("beams daemon: OpenSSH multiplexing available for {}", endpoint.label()),
            Err(unusable) => eprintln!("beams daemon: no OpenSSH multiplexing for {}: {}", endpoint.label(), unusable.reason),
        }
        probes.insert(endpoint.clone(), Probe { stamp, checked: Instant::now(), outcome: outcome.clone() });
        outcome
    }

    /// Generates the endpoint's OpenSSH config and checks that OpenSSH can
    /// use the profile's key.
    fn probe(&self, endpoint: &Endpoint) -> Result<Profile, Unusable> {
        let output = Tsh::new(self.tsh_program.clone(), Some(endpoint.clone()))
            .command()
            .arg("config")
            .stdin(Stdio::null())
            .output()
            .map_err(|err| Unusable { reason: format!("running tsh config: {err}"), transient: false })?;
        if !output.status.success() {
            let stderr = crate::endpoint::plain(&String::from_utf8_lossy(&output.stderr));
            return Err(Unusable { reason: format!("tsh config failed: {stderr}"), transient: true });
        }
        let config = String::from_utf8_lossy(&output.stdout).into_owned();
        let identity = config_value(&config, "IdentityFile")
            .ok_or_else(|| Unusable { reason: "tsh config named no IdentityFile".into(), transient: false })?;
        key_is_usable(Path::new(&identity)).map_err(|reason| Unusable { reason, transient: false })?;
        let path = self.root.join("cfg").join(format!("{}.conf", short_hash(&format!("{endpoint:?}"))));
        write_private(&path, config.as_bytes())
            .map_err(|err| Unusable { reason: format!("{err:#}"), transient: true })?;
        Ok(Profile { config: path, certificate: config_value(&config, "CertificateFile").map(PathBuf::from) })
    }

    fn start(&self, target: &BeamTarget, endpoint: &Endpoint, profile: &Profile) -> Result<Live, Failure> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);
        let stable = self.stable_socket(&target.beam_uuid);
        let socket = PathBuf::from(format!("{}.{generation}", stable.display()));
        let _ = fs::remove_file(&socket);
        let host = format!("{}.{}", target.beam_uuid, endpoint.cluster);
        let mut command = Command::new(&self.ssh_program);
        endpoint.apply(&mut command);
        command
            .arg("-F")
            .arg(&profile.config)
            .args(["-M", "-N", "-S"])
            .arg(&socket)
            .args([
                "-o", "ControlPersist=no",
                "-o", "BatchMode=yes",
                "-o", "StrictHostKeyChecking=yes",
                "-o", "ServerAliveInterval=15",
                "-o", "ServerAliveCountMax=3",
                // Forward failures are reported to `-O forward`; they must
                // not take the whole master down.
                "-o", "ExitOnForwardFailure=no",
                "-o", "LogLevel=ERROR",
            ])
            .arg(format!("{}@{host}", target.login))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        crate::forward::configure_parent_death_signal(&mut command);
        let mut child = self.spawn(command).map_err(|err| Failure {
            reason: format!("starting {}: {err}", self.ssh_program),
            profile_unusable: true,
        })?;
        let diagnostics = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = child.stderr.take() {
            let diagnostics = Arc::clone(&diagnostics);
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut lines = diagnostics.lock().unwrap();
                    if lines.len() == DIAGNOSTIC_LINES {
                        lines.pop_front();
                    }
                    lines.push_back(line);
                }
            });
        }
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                // Give the stderr reader a moment to catch the last lines.
                thread::sleep(Duration::from_millis(50));
                let detail = diagnostics.lock().unwrap().iter().cloned().collect::<Vec<_>>().join(" | ");
                let lower = detail.to_ascii_lowercase();
                let profile_unusable = lower.contains("permission denied")
                    || lower.contains("host key verification failed")
                    || lower.contains("no such file");
                let _ = fs::remove_file(&socket);
                return Err(Failure {
                    reason: if detail.is_empty() {
                        format!("SSH master exited with {status}")
                    } else {
                        format!("SSH master exited with {status}: {detail}")
                    },
                    profile_unusable,
                });
            }
            if socket.exists() && self.control(&socket, "check").unwrap_or(false) {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&socket);
                return Err(Failure { reason: "SSH master didn't connect in time".into(), profile_unusable: false });
            }
            thread::sleep(Duration::from_millis(25));
        }
        // Repoint the stable name at this generation.
        let temp = PathBuf::from(format!("{}.link", stable.display()));
        let _ = fs::remove_file(&temp);
        if std::os::unix::fs::symlink(&socket, &temp).is_ok() {
            let _ = fs::rename(&temp, &stable);
        }
        let expires_at = profile.certificate.as_deref().and_then(certificate_expiry);
        let rotate_before = *self.rotate_before.lock().unwrap();
        Ok(Live {
            child,
            lease: Lease { socket, host, alive: Arc::new(AtomicBool::new(true)) },
            rotate_at: expires_at.map(|at| at.checked_sub(rotate_before).unwrap_or(at)),
            expires_at,
            next_renewal_check: Instant::now(),
        })
    }

    fn spawn(&self, command: Command) -> std::io::Result<Child> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.spawner
            .lock()
            .unwrap()
            .send((command, reply_tx))
            .map_err(|_| std::io::Error::other("SSH master spawner stopped"))?;
        reply_rx.recv().map_err(|_| std::io::Error::other("SSH master spawner stopped"))?
    }

    /// `ssh -O <op>` on a control socket. `Ok(true)` when it succeeded.
    fn control(&self, socket: &Path, op: &str) -> std::io::Result<bool> {
        Command::new(&self.ssh_program)
            .args(["-F", "none", "-S"])
            .arg(socket)
            .args(["-O", op, CONTROL_HOST])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
    }

    fn close_stale_masters(&self) {
        let Ok(entries) = fs::read_dir(&self.sockets) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| !kind.is_symlink()) {
                let _ = self.control(&path, "exit");
            }
            let _ = fs::remove_file(&path);
        }
    }

    fn maintain_once(&self) {
        let now = SystemTime::now();
        let slots: Vec<_> = self.slots.lock().unwrap().values().cloned().collect();
        for slot in slots {
            // A slot being started is busy, not dead.
            let Ok(mut slot) = slot.try_lock() else { continue };
            let Slot::Ready(live) = &mut *slot else { continue };
            if !matches!(live.child.try_wait(), Ok(None)) {
                eprintln!("beams daemon: SSH master to {} exited", live.lease.host);
                live.lease.alive.store(false, Ordering::SeqCst);
                let _ = fs::remove_file(&live.lease.socket);
                *slot = Slot::Failed { until: Instant::now(), delay: RETRY_MIN, reason: "SSH master exited".into() };
            } else if live.expires_at.is_some_and(|at| now >= at) {
                eprintln!("beams daemon: closing the SSH master to {}: its certificate expired", live.lease.host);
                let Slot::Ready(old) = std::mem::replace(&mut *slot, Slot::Empty) else { unreachable!() };
                self.kill(old);
            }
        }
        let expired: Vec<Live> = {
            let mut draining = self.draining.lock().unwrap();
            let mut expired = Vec::new();
            let mut keep = Vec::new();
            for mut live in std::mem::take(&mut *draining) {
                let gone = !matches!(live.child.try_wait(), Ok(None));
                if gone || live.expires_at.is_none_or(|at| now >= at) {
                    expired.push(live);
                } else {
                    keep.push(live);
                }
            }
            *draining = keep;
            expired
        };
        for live in expired {
            self.kill(live);
        }
    }
}

fn maintain(pool: Weak<Pool>) {
    loop {
        thread::sleep(MAINTENANCE_INTERVAL);
        let Some(pool) = pool.upgrade() else { return };
        if pool.stopped.load(Ordering::SeqCst) {
            return;
        }
        pool.maintain_once();
    }
}

struct Failure {
    reason: String,
    /// Retrying won't help until the profile changes (`tsh login`).
    profile_unusable: bool,
}

/// Tier B with per-operation Tier C fallback. Which one an operation uses is
/// decided when its command is built, so a master that comes up (or dies)
/// later is picked up by the next operation.
#[derive(Debug)]
struct MultiplexedConnection {
    target: BeamTarget,
    pool: Arc<Pool>,
    fallback: TshConnection,
}

impl MultiplexedConnection {
    /// `ssh` attached to `lease`'s master. It reads no config and can't open
    /// a connection of its own: if the master is gone it fails instead of
    /// dialing anything directly.
    fn client(&self, program: &str, lease: &Lease) -> Command {
        let mut command = Command::new(program);
        command
            .args(["-F", "none", "-o"])
            .arg(control_path_option(&lease.socket))
            .args(["-o", "ControlMaster=no", "-o", "ProxyCommand=false", "-o", "BatchMode=yes"]);
        command
    }
}

impl BeamConnection for MultiplexedConnection {
    fn target(&self) -> &BeamTarget {
        &self.target
    }

    fn exec(&self, remote_command: &str) -> Command {
        match self.pool.lease(&self.target) {
            Ok(lease) => {
                let mut command = self.client(&self.pool.ssh_program, &lease);
                command.arg("-T").arg(format!("{}@{}", self.target.login, lease.host)).arg("--").arg(remote_command);
                command
            }
            Err(_) => self.fallback.exec(remote_command),
        }
    }

    fn local_forward(&self, local_port: u16, remote_host: &str, remote_port: u16) -> ForwardPlan {
        match self.pool.lease(&self.target) {
            Ok(lease) => {
                let spec = format!("127.0.0.1:{local_port}:{remote_host}:{remote_port}");
                let control = |op: &str| {
                    let mut command = self.client(&self.pool.ssh_program, &lease);
                    command.args(["-O", op, "-L", &spec, CONTROL_HOST]);
                    command
                };
                ForwardPlan::Master { open: control("forward"), cancel: control("cancel"), lease: lease.clone() }
            }
            Err(_) => self.fallback.local_forward(local_port, remote_host, remote_port),
        }
    }

    fn copy(&self, recursive: bool, source: &OsStr, destination: &OsStr) -> Command {
        match self.pool.lease(&self.target) {
            Ok(lease) => {
                let mut command = self.client(&self.pool.scp_program, &lease);
                if recursive {
                    command.arg("-r");
                }
                command.arg(source).arg(destination);
                command
            }
            Err(_) => self.fallback.copy(recursive, source, destination),
        }
    }

    fn git_remote(&self, git: &mut Command, remote_path: &str) -> String {
        // The CLI pushes before the daemon owns the Beam; stays on tsh.
        self.fallback.git_remote(git, remote_path)
    }

    fn mutagen_host_entry(&self) -> Option<String> {
        let entry = self.fallback.mutagen_host_entry()?;
        let endpoint = self.target.endpoint.as_ref()?;
        if !self.pool.supports(endpoint) {
            return Some(entry);
        }
        // Make sure there's a master to find; the wrapper still checks it
        // per connection and falls back to tsh without one.
        let _ = self.pool.lease(&self.target);
        Some(format!("{entry}{}\n", self.pool.stable_socket(&self.target.beam_uuid).display()))
    }
}

fn control_path_option(socket: &Path) -> OsString {
    let mut option = OsString::from("ControlPath=");
    // `%` starts an OpenSSH token in ControlPath.
    option.push(socket.as_os_str().to_string_lossy().replace('%', "%%"));
    option
}

/// The value of the first `key "value"` (or unquoted) line in an OpenSSH
/// config.
fn config_value(config: &str, key: &str) -> Option<String> {
    config.lines().find_map(|line| {
        let (name, value) = line.trim().split_once(char::is_whitespace)?;
        name.eq_ignore_ascii_case(key).then(|| value.trim().trim_matches('"').to_string())
    })
}

/// OpenSSH can only authenticate with a file key; a hardware-key reference
/// in the profile means `tsh` must do the signing.
fn key_is_usable(path: &Path) -> Result<(), String> {
    let text = fs::read(path).map_err(|err| format!("reading the profile key {}: {err}", path.display()))?;
    let first = String::from_utf8_lossy(&text[..text.len().min(128)]).lines().next().unwrap_or_default().to_string();
    const FILE_KEYS: [&str; 4] = ["OPENSSH PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY", "-----BEGIN PRIVATE KEY"];
    if FILE_KEYS.iter().any(|kind| first.contains(kind)) {
        Ok(())
    } else if first.contains("PIV") || first.contains("HARDWARE") {
        Err("the profile key is hardware-backed; OpenSSH can't use it".into())
    } else {
        Err(format!("unrecognized profile key format ({first:?})"))
    }
}

/// When the profile last changed: `tsh login` rewrites `<proxy host>.yaml`.
fn profile_stamp(endpoint: &Endpoint) -> Option<SystemTime> {
    let home = match &endpoint.teleport_home {
        Some(home) => home.clone(),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".tsh"),
    };
    let host = endpoint.proxy.rsplit_once(':').map_or(endpoint.proxy.as_str(), |(host, _)| host);
    fs::metadata(home.join(format!("{host}.yaml"))).and_then(|meta| meta.modified()).ok()
}

/// `Valid: from ... to ...` from `ssh-keygen -L`, in UTC.
fn certificate_expiry(path: &Path) -> Option<SystemTime> {
    let output = Command::new("ssh-keygen").env("TZ", "UTC").arg("-L").arg("-f").arg(path).output().ok()?;
    parse_validity(&String::from_utf8_lossy(&output.stdout))
}

fn parse_validity(listing: &str) -> Option<SystemTime> {
    let line = listing.lines().find(|line| line.trim_start().starts_with("Valid:"))?;
    let to = line.split(" to ").nth(1)?.trim();
    let at = chrono::NaiveDateTime::parse_from_str(to, "%Y-%m-%dT%H:%M:%S").ok()?;
    let secs = u64::try_from(at.and_utc().timestamp()).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// FNV-1a, for short file names (not security-relevant).
fn short_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{:012x}", hash & 0xffff_ffff_ffff)
}

fn private_dir(path: &Path) -> anyhow::Result<PathBuf> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let meta = fs::symlink_metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {}", path.display()))?;
    }
    Ok(path.to_path_buf())
}

fn write_private(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let temp = path.with_extension("tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("writing {}", temp.display()))?;
    file.write_all(contents)?;
    fs::rename(&temp, path).with_context(|| format!("installing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_values_from_tsh_config_output() {
        let config = "Host *.example.com proxy.example.com\n    UserKnownHostsFile \"/h/.tsh/known_hosts\"\n    IdentityFile \"/h/.tsh/keys/example.com/bob\"\n    CertificateFile \"/h/.tsh/keys/example.com/bob-ssh/example.com-cert.pub\"\n";
        assert_eq!(config_value(config, "IdentityFile").as_deref(), Some("/h/.tsh/keys/example.com/bob"));
        assert_eq!(
            config_value(config, "certificatefile").as_deref(),
            Some("/h/.tsh/keys/example.com/bob-ssh/example.com-cert.pub")
        );
        assert_eq!(config_value(config, "ProxyCommand"), None);
    }

    #[test]
    fn hardware_keys_are_not_usable_by_openssh() {
        let dir = std::env::temp_dir().join(format!("beams-master-key-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file");
        fs::write(&file, "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n").unwrap();
        assert!(key_is_usable(&file).is_ok());
        let piv = dir.join("piv");
        fs::write(&piv, "-----BEGIN PIV YUBIKEY PRIVATE KEY-----\nabc\n").unwrap();
        assert!(key_is_usable(&piv).unwrap_err().contains("hardware-backed"));
        assert!(key_is_usable(&dir.join("missing")).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_certificate_validity() {
        let listing = "        Type: ssh-ed25519-cert-v01@openssh.com user certificate\n        Valid: from 2026-11-12T10:00:00 to 2026-11-12T22:00:00\n";
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 11, 12).unwrap().and_hms_opt(22, 0, 0).unwrap().and_utc().timestamp();
        assert_eq!(parse_validity(listing), Some(SystemTime::UNIX_EPOCH + Duration::from_secs(expected as u64)));
        assert_eq!(parse_validity("        Valid: forever\n"), None);
    }

    #[test]
    fn short_hashes_are_stable_and_short() {
        assert_eq!(short_hash("abc"), short_hash("abc"));
        assert_ne!(short_hash("abc"), short_hash("abd"));
        assert_eq!(short_hash("abc").len(), 12);
    }

    /// End to end against a real, unprivileged `sshd` (opt-in: set
    /// `BEAMS_TEST_SSHD` to an sshd binary; `sshd-session`, `sshd-auth` and
    /// `sftp-server` are looked up in `../lib/openssh` next to it). A fake `tsh` generates the
    /// config and relays `proxy ssh` to the sshd, checking the pin.
    #[test]
    fn multiplexes_against_a_local_sshd() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let Some(sshd) = std::env::var_os("BEAMS_TEST_SSHD").map(PathBuf::from) else {
            eprintln!("skipping: BEAMS_TEST_SSHD not set");
            return;
        };
        let libexec = sshd.parent().unwrap().join("../lib/openssh");
        let dir = PathBuf::from(format!("/tmp/bm-{}", &uuid::Uuid::new_v4().to_string()[..8]));
        fs::create_dir_all(&dir).unwrap();
        let d = dir.display().to_string();
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap().trim().to_string();
        let run = |args: &[&str]| assert!(Command::new(args[0]).args(&args[1..]).status().unwrap().success(), "{args:?}");
        run(&["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", &format!("{d}/host")]);
        run(&["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", &format!("{d}/ca")]);
        run(&["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", &format!("{d}/user")]);
        // Expires in 90 s, so rotation (below) and expiry parsing are real.
        run(&["ssh-keygen", "-q", "-s", &format!("{d}/ca"), "-I", "test", "-n", &user, "-V", "-1m:+90s", &format!("{d}/user.pub")]);
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        fs::write(
            dir.join("sshd_config"),
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host\nTrustedUserCAKeys {d}/ca.pub\nAuthorizedKeysFile none\n\
                 PidFile {d}/sshd.pid\nUsePAM no\nStrictModes no\nAllowTcpForwarding yes\nMaxSessions 20\n\
                 SshdSessionPath {0}/sshd-session\nSshdAuthPath {0}/sshd-auth\nSubsystem sftp {0}/sftp-server\n",
                libexec.display()
            ),
        )
        .unwrap();
        run(&[sshd.to_str().unwrap(), "-f", &format!("{d}/sshd_config"), "-E", &format!("{d}/sshd.log")]);
        // Stops sshd and removes the scratch directory even if an assertion fails.
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Ok(pid) = fs::read_to_string(self.0.join("sshd.pid")) {
                    let _ = Command::new("kill").arg(pid.trim()).status();
                }
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let host_key = fs::read_to_string(dir.join("host.pub")).unwrap();
        fs::write(dir.join("known_hosts"), format!("*.test.example.com {host_key}")).unwrap();
        let tsh = dir.join("tsh");
        fs::write(
            &tsh,
            format!(
                r#"#!/bin/bash
[ "$TELEPORT_PROXY" = test.example.com:443 ] && [ "$TELEPORT_CLUSTER" = test.example.com ] || {{ echo "unpinned: $*" >&2; exit 98; }}
case "$1" in
config) cat <<CFG
Host *.test.example.com
    UserKnownHostsFile "{d}/known_hosts"
    IdentityFile "{d}/user"
    CertificateFile "{d}/user-cert.pub"
    Port {port}
    ProxyCommand "{d}/tsh" proxy ssh --cluster=test.example.com --proxy=test.example.com:443 %r@%h:%p
CFG
;;
proxy) echo dial >> "{d}/dials"; exec 3<>/dev/tcp/127.0.0.1/{port}; cat <&3 & exec cat >&3 ;;
*) echo "fake tsh: $*" >&2; exit 97 ;;
esac
"#
            ),
        )
        .unwrap();
        fs::set_permissions(&tsh, fs::Permissions::from_mode(0o700)).unwrap();
        let dials = || fs::read_to_string(dir.join("dials")).unwrap_or_default().lines().count();

        let pool = Pool::with_programs(dir.join("rt"), tsh.display().to_string(), "ssh".into(), "scp".into()).unwrap();
        // Rotate 5 s after start, well before the 90 s expiry.
        *pool.rotate_before.lock().unwrap() = Duration::from_secs(85);
        let target = BeamTarget {
            beam_id: "beam".into(),
            beam_uuid: "beam-uuid".into(),
            login: user.clone(),
            endpoint: Some(Endpoint {
                proxy: "test.example.com:443".into(),
                cluster: "test.example.com".into(),
                user: String::new(),
                teleport_home: None,
            }),
        };
        let connection = pool.connection(tsh.to_str().unwrap(), target.clone());

        // Commands run as channels on one master: one proxy dial for all.
        let started = Instant::now();
        for _ in 0..10 {
            let output = connection.exec("echo hi").output().unwrap();
            assert_eq!(String::from_utf8_lossy(&output.stdout), "hi\n", "{}", String::from_utf8_lossy(&output.stderr));
        }
        eprintln!("10 commands in {:?}", started.elapsed());
        assert_eq!(connection.exec("true").get_program(), "ssh");
        assert_eq!(dials(), 1);

        // Copies.
        fs::write(dir.join("src"), "payload").unwrap();
        let remote = target.remote_path(&format!("{d}/dst"));
        assert!(connection.copy(false, dir.join("src").as_os_str(), &remote).status().unwrap().success());
        assert_eq!(fs::read_to_string(dir.join("dst")).unwrap(), "payload");

        // Forwards live on the master: open, use, cancel.
        let service = TcpListener::bind("127.0.0.1:0").unwrap();
        let service_port = service.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in service.incoming().flatten() {
                let mut stream = stream;
                let _ = stream.write_all(b"from-beam");
            }
        });
        let local_port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let ForwardPlan::Master { mut open, mut cancel, lease } = connection.local_forward(local_port, "127.0.0.1", service_port) else {
            panic!("expected a master forward");
        };
        assert!(open.status().unwrap().success());
        let mut reply = String::new();
        std::net::TcpStream::connect(("127.0.0.1", local_port)).unwrap().read_to_string(&mut reply).unwrap();
        assert_eq!(reply, "from-beam");
        assert!(cancel.status().unwrap().success());
        thread::sleep(Duration::from_millis(100));
        assert!(std::net::TcpStream::connect(("127.0.0.1", local_port)).is_err());

        // Mutagen's record points at the stable socket, which works.
        let entry = connection.mutagen_host_entry().unwrap();
        let stable = entry.lines().nth(3).unwrap();
        assert!(pool.control(Path::new(stable), "check").unwrap());

        // Near expiry without a new login, the master stays in use.
        thread::sleep(Duration::from_secs(6));
        assert_eq!(pool.lease(&target).unwrap().socket, lease.socket);
        // After one (a longer certificate), new work moves to a new master
        // and the old one keeps its channels until its own expiry.
        run(&["ssh-keygen", "-q", "-s", &format!("{d}/ca"), "-I", "test", "-n", &user, "-V", "-1m:+10m", &format!("{d}/user.pub")]);
        *pool.rotate_before.lock().unwrap() = Duration::from_secs(60);
        if let Some(Slot::Ready(live)) = pool.slots.lock().unwrap().get("beam-uuid").map(|slot| slot.lock().unwrap()).as_deref_mut() {
            live.next_renewal_check = Instant::now();
        }
        let second = pool.lease(&target).unwrap();
        assert_eq!(pool.lease(&target).unwrap().socket, second.socket, "no churn on the renewed certificate");
        assert_ne!(second.socket, lease.socket);
        assert!(lease.is_alive() && second.is_alive());
        assert!(pool.control(&lease.socket, "check").unwrap());
        assert_eq!(dials(), 2);

        // A master that dies is noticed and replaced on next use.
        let pid = String::from_utf8(
            Command::new("ssh").args(["-F", "none", "-S"]).arg(&second.socket).args(["-O", "check", CONTROL_HOST]).output().unwrap().stderr,
        )
        .unwrap();
        let pid = pid.split("pid=").nth(1).unwrap().trim_end_matches([')', '\n', '\r']).to_string();
        run(&["kill", &pid]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while second.is_alive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        assert!(!second.is_alive());
        assert!(connection.exec("true").status().unwrap().success());
        assert_eq!(dials(), 3);

        let kill_master = |socket: &Path| {
            let out = Command::new("ssh").args(["-F", "none", "-S"]).arg(socket).args(["-O", "check", CONTROL_HOST]).output().unwrap();
            let text = String::from_utf8(out.stderr).unwrap();
            let pid = text.split("pid=").nth(1).unwrap().trim_end_matches([')', '\n', '\r']).to_string();
            run(&["kill", &pid]);
        };

        // Mutagen's wrappers use the master when it's up.
        let transport = crate::sync::prepare_transport(&dir.join("mutagen")).unwrap();
        crate::sync::write_endpoint(&transport, connection.as_ref()).unwrap();
        let wrapped = Command::new(transport.bin.join("ssh"))
            .args(["-oControlMaster=no", &format!("{user}@beam-uuid"), "echo", "wrapped"])
            .env("BEAMS_TSH", "/nonexistent-tsh")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&wrapped.stdout), "wrapped\n", "{}", String::from_utf8_lossy(&wrapped.stderr));
        fs::write(dir.join("agent"), "agent-binary").unwrap();
        let copied = Command::new(transport.bin.join("scp"))
            .args(["-q", dir.join("agent").to_str().unwrap(), &format!("{user}@beam-uuid:{d}/agent-copy")])
            .env("BEAMS_TSH", "/nonexistent-tsh")
            .status()
            .unwrap();
        assert!(copied.success());
        assert_eq!(fs::read_to_string(dir.join("agent-copy")).unwrap(), "agent-binary");

        // A recursive fetch, through the copy worker.
        fs::create_dir_all(dir.join("tree/sub")).unwrap();
        fs::write(dir.join("tree/sub/f"), "leaf").unwrap();
        let (events, event_rx) = mpsc::sync_channel(64);
        let _fetch = crate::copy::spawn_fetch(
            Arc::clone(&connection),
            "fetch".into(),
            beams_protocol::ServiceRequestKind::FileFetch {
                sources: vec![beams_protocol::FetchSource { path: format!("{d}/tree"), size: 4, is_dir: true }],
                local_path_hint: None,
                overwrite: beams_protocol::OverwritePolicy::Deny,
            },
            dir.join("fetched"),
            events,
        );
        loop {
            let event = event_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if event.status == beams_protocol::ServiceStatus::Stopped || event.status == beams_protocol::ServiceStatus::Error {
                assert_eq!(event.status, beams_protocol::ServiceStatus::Stopped, "{:?}", event.message);
                break;
            }
        }
        assert_eq!(fs::read_to_string(dir.join("fetched/tree/sub/f")).unwrap(), "leaf");

        // The forward worker re-opens a forward whose master went away.
        let (events, event_rx) = mpsc::sync_channel(64);
        let actions = crate::forward::spawn(
            Arc::clone(&connection),
            "forward".into(),
            beams_protocol::ServiceRequestKind::LocalPortForward { remote_host: "127.0.0.1".into(), remote_port: service_port, local_port: None },
            events,
        );
        let running_port = |rx: &mpsc::Receiver<crate::forward::Event>| loop {
            let event = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            match (event.status, event.kind) {
                (beams_protocol::ServiceStatus::Running, beams_protocol::ServiceRequestKind::LocalPortForward { local_port: Some(port), .. }) => return port,
                (beams_protocol::ServiceStatus::Error, _) => panic!("forward failed: {:?}", event.message),
                _ => {}
            }
        };
        let port = running_port(&event_rx);
        let read = |port: u16| {
            let mut reply = String::new();
            std::net::TcpStream::connect(("127.0.0.1", port)).unwrap().read_to_string(&mut reply).unwrap();
            reply
        };
        assert_eq!(read(port), "from-beam");
        kill_master(&pool.lease(&target).unwrap().socket);
        assert_eq!(running_port(&event_rx), port, "re-opened on the same port");
        assert_eq!(read(port), "from-beam");
        actions.send(crate::forward::Action::Stop).unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "cancelled on stop");

        pool.shutdown();
        assert!(!lease.is_alive());
        assert_eq!(connection.exec("true").get_program(), tsh.as_os_str(), "falls back to tsh");
    }
}
