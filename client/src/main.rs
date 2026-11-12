mod agent;
mod beamcmd;
mod startup;
mod knownhosts;
mod bridge;
mod clientd;
mod copy;
mod config;
mod daemonctl;
mod connection;
mod master;
mod endpoint;
mod forward;
mod session;
mod stage;
mod state;
mod sync;
mod terminal_eligibility;
mod terminal_model;
mod terminal_paint;
mod terminal_stream;
mod tsh;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context};
use beams_protocol::{
    FileCopyDirection, FolderSyncMode, OverwritePolicy, ServiceAction, ServiceRequestKind,
    ServiceStatus,
};
use chrono::{DateTime, Duration, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use uuid::Uuid;

/// A tracked Beam is reused only with more TTL than this left; `--beam`
/// connects regardless.
const MIN_TTL_HOURS: i64 = 6;
const MAX_CONCURRENT_BEAMS: usize = 5;
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
const DEFAULT_LOGIN: &str = "beams";
const DEFAULT_PURPOSE: &str = "default";
/// Where `--push-repo` puts the checkout on the beam, relative to its
/// `$HOME` (`~/beams`).
const REMOTE_REPO_PATH: &str = "beams";

/// Spawn or reuse a Teleport Beam and drop straight into it.
#[derive(Parser)]
#[command(name = "beams")]
struct Args {
    /// Use the separate `dev` client instance: its own local daemon, daemon
    /// state, launches and Beams, sharing only `config.toml` and profiles.
    /// For testing a development build without touching the daemon your
    /// sessions use. Same as `BEAMS_INSTANCE=dev`.
    #[arg(long, global = true)]
    dev: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Inspect configurable launch profiles offline (live launch wiring pending).
    Config {
        #[command(subcommand)]
        command: config::Command,
    },
    /// Connect to a beam, spawning or reusing one as needed.
    Ssh {
        /// The beams repo checkout: where the Beam runtime's
        /// `beam-bootstrap` build comes from (and what `--push-repo`
        /// pushes). Ignored when reusing an existing beam.
        #[arg(long, default_value = "~/beams")]
        repo: String,
        /// Named purpose this beam serves. Lets more than one beam be
        /// tracked independently - e.g. a side task vs. the default one
        /// plain `beams ssh` reuses.
        #[arg(long, default_value = DEFAULT_PURPOSE)]
        purpose: String,
        /// Region to request if a new beam needs to be created.
        #[arg(long)]
        region: Option<String>,
        /// Login to connect as.
        #[arg(long, default_value = DEFAULT_LOGIN)]
        login: String,
        /// Local prefix key that opens the Beams service menu. Press it
        /// twice to send the key to the remote session. Accepts
        /// values such as `ctrl-]`, `ctrl-g`, `0x1d`, one ASCII
        /// character, or `none`.
        #[arg(long, default_value = "ctrl-]")]
        escape_key: session::EscapeKey,
        /// Experimental: keep a shadow terminal model and paint the local
        /// menu over the captured remote screen instead of a blank takeover.
        #[arg(long)]
        terminal_overlay: bool,
        /// Launch a new Beam with this profile (`builtin:NAME` or a TOML
        /// path). Default: `[launch] profile` in config.toml, else
        /// `builtin:development`. A reused Beam must have been launched
        /// with the same profile.
        #[arg(long)]
        profile: Option<String>,
        /// Also push the repo checkout to `~/beams` on a new Beam (the
        /// launch itself doesn't need it).
        #[arg(long)]
        push_repo: bool,
        /// Connect to this existing Beam (id, uuid, or tracked purpose)
        /// however little TTL it has left; never creates one or changes
        /// which Beam a purpose tracks.
        #[arg(long, conflicts_with_all = ["purpose", "region", "push_repo"])]
        beam: Option<String>,
    },
    /// Run the workstation-side service daemon in the foreground. Normal
    /// `beams ssh` invocations start it automatically as needed.
    ///
    /// The daemon restarts itself into a rebuilt or reinstalled `beams`
    /// binary once the new file is stable and nothing would be lost (no
    /// file copy in flight, no approval pending).
    Daemon {
        /// Override the local Unix socket path.
        #[arg(long, global = true)]
        socket: Option<PathBuf>,
        #[command(subcommand)]
        action: Option<DaemonAction>,
    },
    /// Upload one local file to an existing tracked Beam.
    CopyTo {
        #[arg(long, default_value = DEFAULT_PURPOSE)]
        purpose: String,
        #[arg(long, default_value = DEFAULT_LOGIN)]
        login: String,
        local_path: String,
        remote_path: String,
        #[arg(long)]
        overwrite: bool,
    },
    /// Download one file from an existing tracked Beam.
    CopyFrom {
        #[arg(long, default_value = DEFAULT_PURPOSE)]
        purpose: String,
        #[arg(long, default_value = DEFAULT_LOGIN)]
        login: String,
        remote_path: String,
        local_path: String,
        #[arg(long)]
        overwrite: bool,
    },
    /// Create, inspect, or control Mutagen folder syncs.
    Sync {
        #[command(subcommand)]
        command: SyncCommands,
    },
    /// List your beams, with the purpose each one is tracked for.
    Ls {
        /// Include beams owned by other users (`tsh beams ls --all`).
        #[arg(long)]
        all: bool,
    },
    /// Delete beams and forget any purposes tracking them.
    Rm {
        /// Beam ids, uuids, or tracked purpose names.
        #[arg(required_unless_present = "prune")]
        targets: Vec<String>,
        /// Also forget tracked purposes whose beam no longer exists.
        #[arg(long)]
        prune: bool,
    },
    /// Run a command in a beam (`tsh beams exec`).
    Exec {
        /// Beam id, uuid, or tracked purpose (default: the `default` purpose).
        target: Option<String>,
        #[arg(long, default_value = DEFAULT_LOGIN)]
        login: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Copy files to or from a beam (`tsh beams scp`). Remote operands
    /// are `TARGET:PATH`, where TARGET is a beam id, uuid, or tracked
    /// purpose; `:PATH` alone means the `default` purpose.
    Scp {
        #[arg(short, long)]
        recursive: bool,
        #[arg(short, long)]
        quiet: bool,
        src: String,
        dest: String,
    },
    /// Publish a beam's port 8080 as a Teleport app (`tsh beams publish`).
    Publish {
        /// Beam id, uuid, or tracked purpose (default: the `default` purpose).
        target: Option<String>,
        /// Publish as a TCP app instead of an HTTP app.
        #[arg(long)]
        tcp: bool,
    },
    /// Unpublish a beam's previously published app (`tsh beams unpublish`).
    Unpublish {
        /// Beam id, uuid, or tracked purpose (default: the `default` purpose).
        target: Option<String>,
    },
    /// Open an IDE with a remote session into a beam (`tsh beams ide`).
    Ide {
        /// Beam id, uuid, or tracked purpose (default: the `default` purpose).
        target: Option<String>,
        /// IDE to open (vscode, cursor, zed); tsh prompts when omitted.
        #[arg(long)]
        ide: Option<String>,
    },
    /// List the keys in this terminal's SSH agent and whether Beams may use
    /// them (`[ssh_agent] allowed_keys` in the client config).
    AgentKeys {
        /// Let Beams ask to use this key (its number from the listing, or
        /// its SHA256 fingerprint).
        #[arg(long, value_name = "N|FINGERPRINT")]
        allow: Option<String>,
        /// Stop sharing this key with Beams.
        #[arg(long, value_name = "N|FINGERPRINT")]
        deny: Option<String>,
    },
}

#[derive(Subcommand)]
enum SyncCommands {
    /// Synchronize an existing workstation directory with an existing Beam directory.
    Create {
        #[arg(long, default_value = DEFAULT_PURPOSE)]
        purpose: String,
        #[arg(long, default_value = DEFAULT_LOGIN)]
        login: String,
        #[arg(long, value_enum, default_value_t = SyncModeArg::TwoWaySafe)]
        mode: SyncModeArg,
        /// Include VCS metadata such as .git (ignored by default).
        #[arg(long)]
        include_vcs: bool,
        /// Mutagen ignore rule; repeat for multiple rules.
        #[arg(long = "ignore")]
        ignores: Vec<String>,
        local_path: String,
        remote_path: String,
    },
    /// List folder syncs known to the local daemon.
    List,
    /// Pause a folder sync without deleting its Mutagen session.
    Pause { service_id: String },
    /// Resume a paused folder sync.
    Resume { service_id: String },
    /// Permanently terminate a folder sync and its Mutagen session.
    Terminate { service_id: String },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SyncModeArg {
    TwoWaySafe,
    TwoWayResolved,
    OneWaySafe,
    OneWayReplica,
}

impl From<SyncModeArg> for FolderSyncMode {
    fn from(value: SyncModeArg) -> Self {
        match value {
            SyncModeArg::TwoWaySafe => Self::TwoWaySafe,
            SyncModeArg::TwoWayResolved => Self::TwoWayResolved,
            SyncModeArg::OneWaySafe => Self::OneWaySafe,
            SyncModeArg::OneWayReplica => Self::OneWayReplica,
        }
    }
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Show the running daemon's PID, version, executable, and whether it's
    /// busy.
    Status,
    /// Ask the daemon to shut down gracefully. Forwards stop, syncs are
    /// detached (their Mutagen sessions keep running), in-flight copies are
    /// interrupted.
    Stop {
        /// Stop even if a file copy is in flight or an approval is pending.
        #[arg(long)]
        force: bool,
    },
    /// Stop the daemon and start a fresh one from this `beams` binary.
    /// Running `beams ssh` sessions reconnect on their own.
    Restart {
        /// Restart even if a file copy is in flight or an approval is pending.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> anyhow::Result<ExitCode> {
    daemonctl::record_startup_exe();
    let args = Args::parse();
    if args.dev {
        // Before anything resolves a path; a daemon this process starts
        // inherits it.
        std::env::set_var(clientd::INSTANCE_ENV, "dev");
    }
    clientd::instance()?;
    let code = match args.command {
        Commands::Config { command } => config::run(command)?,
        Commands::Ssh {
            repo,
            purpose,
            region,
            login,
            escape_key,
            terminal_overlay,
            profile,
            push_repo,
            beam,
        } => ssh(
            &SshOptions {
                repo: expand_tilde(&repo),
                purpose,
                region,
                login,
                profile,
                push_repo,
                beam,
            },
            escape_key,
            session::DisplayOptions {
                overlay: terminal_overlay,
            },
        )?,
        Commands::AgentKeys { allow, deny } => agent::agent_keys_command(allow.as_deref(), deny.as_deref())?,
        Commands::Daemon { socket, action } => match action {
            None => {
                clientd::serve(socket)?;
                0
            }
            Some(DaemonAction::Status) => daemonctl::print_status(socket)?,
            Some(DaemonAction::Stop { force }) => daemonctl::run_stop(socket, force)?,
            Some(DaemonAction::Restart { force }) => daemonctl::run_restart(socket, force)?,
        },
        Commands::CopyTo {
            purpose,
            login,
            local_path,
            remote_path,
            overwrite,
        } => copy_file(
            &purpose,
            &login,
            FileCopyDirection::Upload,
            &local_path,
            &remote_path,
            overwrite,
        )?,
        Commands::CopyFrom {
            purpose,
            login,
            remote_path,
            local_path,
            overwrite,
        } => copy_file(
            &purpose,
            &login,
            FileCopyDirection::Download,
            &local_path,
            &remote_path,
            overwrite,
        )?,
        Commands::Sync { command } => match command {
            SyncCommands::Create {
                purpose,
                login,
                mode,
                include_vcs,
                ignores,
                local_path,
                remote_path,
            } => create_folder_sync(
                &purpose,
                &login,
                &local_path,
                &remote_path,
                mode.into(),
                !include_vcs,
                ignores,
            )?,
            SyncCommands::List => list_folder_syncs()?,
            SyncCommands::Pause { service_id } => {
                control_folder_sync(&service_id, ServiceAction::Pause)?
            }
            SyncCommands::Resume { service_id } => {
                control_folder_sync(&service_id, ServiceAction::Resume)?
            }
            SyncCommands::Terminate { service_id } => {
                control_folder_sync(&service_id, ServiceAction::Terminate)?
            }
        },
        Commands::Ls { all } => beamcmd::ls(all)?,
        Commands::Rm { targets, prune } => beamcmd::rm(&targets, prune)?,
        Commands::Exec { target, login, command } => beamcmd::exec(target.as_deref(), &login, &command)?,
        Commands::Scp { recursive, quiet, src, dest } => beamcmd::scp(recursive, quiet, &src, &dest)?,
        Commands::Publish { target, tcp } => beamcmd::publish(target.as_deref(), tcp)?,
        Commands::Unpublish { target } => beamcmd::unpublish(target.as_deref())?,
        Commands::Ide { target, ide } => beamcmd::ide(target.as_deref(), ide.as_deref())?,
    };
    Ok(ExitCode::from(code))
}

fn tracked_target(purpose: &str, login: &str) -> anyhow::Result<connection::BeamTarget> {
    let mut state = state::State::load()?;
    if !state.purposes.contains_key(purpose) {
        bail!("no Beam is tracked for purpose {purpose:?}; run `beams ssh --purpose {purpose}` first");
    }
    let located = beamcmd::locate(purpose, &mut state)
        .with_context(|| format!("the Beam tracked for purpose {purpose:?} is not reachable"))?;
    Ok(connection::BeamTarget {
        beam_id: located.beam.id,
        beam_uuid: located.beam.uuid,
        login: login.to_string(),
        endpoint: Some(located.endpoint),
    })
}

/// The Beam tracked for `purpose`, looked up on the endpoint it lives on,
/// if it can be reused. A purpose pinned to another cluster whose profile
/// has since been logged out is reported and treated as unusable, so the
/// caller spawns a fresh Beam where the user is logged in now; any other
/// listing failure is an error.
fn reusable_for_purpose(
    state: &mut state::State,
    purpose: &str,
) -> anyhow::Result<Option<(endpoint::Endpoint, tsh::BeamInfo, state::PurposeLaunch)>> {
    let Some(entry) = state.purposes.get(purpose) else {
        return Ok(None);
    };
    let (endpoint, pinned) = match &entry.endpoint {
        Some(endpoint) => (endpoint.clone(), true),
        None => (endpoint::Endpoint::current()?, false),
    };
    let beams = match tsh::list_beams(&endpoint) {
        Ok(beams) => beams,
        Err(err) => {
            let logged_out = endpoint.login_hint(&format!("{err:#}")).is_some();
            let elsewhere = pinned && endpoint::Endpoint::current().is_ok_and(|current| current != endpoint);
            if logged_out && elsewhere {
                eprintln!(
                    "beams: not reusing {} for purpose {purpose:?}: its cluster {} is logged out ({})",
                    entry.beam_id,
                    endpoint.label(),
                    endpoint.login_hint("not logged in").unwrap_or_default()
                );
                return Ok(None);
            }
            return Err(err.context(format!("checking purpose {purpose:?}'s Beam on {}", endpoint.label())));
        }
    };
    let Some(beam) = beams.iter().find(|b| b.uuid == entry.uuid).cloned() else {
        return Ok(None);
    };
    let Some(launch) = entry.launch.clone() else {
        eprintln!("beams: not reusing {} for purpose {purpose:?}: it predates profile launches", beam.id);
        return Ok(None);
    };
    if beamcmd::adopt_endpoint(state, &beam.uuid, &endpoint) {
        state.save()?;
    }
    Ok(has_ttl_remaining(&beam, MIN_TTL_HOURS).then_some((endpoint, beam, launch)))
}

fn create_folder_sync(
    purpose: &str,
    login: &str,
    local_path: &str,
    remote_path: &str,
    mode: FolderSyncMode,
    ignore_vcs: bool,
    ignores: Vec<String>,
) -> anyhow::Result<u8> {
    let target = tracked_target(purpose, login)?;
    let mut frontend = clientd::connect_or_start()?;
    let request_id =
        frontend.create_folder_sync(&target, local_path, remote_path, mode, ignore_vcs, ignores)?;
    eprintln!("beams: submitted folder sync {request_id}");
    while let Ok(event) = frontend.events.recv() {
        match event {
            clientd::ServerEvent::ServiceChanged { service }
                if service.request_id == request_id =>
            {
                if let Some(message) = &service.message {
                    eprintln!("beams: {:?}: {message}", service.status);
                }
                return match service.status {
                    ServiceStatus::Running | ServiceStatus::Paused | ServiceStatus::Degraded => {
                        println!("{}", service.service_id);
                        Ok(0)
                    }
                    ServiceStatus::Error | ServiceStatus::Stopped => Ok(1),
                    _ => continue,
                };
            }
            clientd::ServerEvent::Error { message } => bail!("{message}"),
            _ => {}
        }
    }
    bail!("local daemon disconnected before the folder sync started")
}

fn list_folder_syncs() -> anyhow::Result<u8> {
    let frontend = clientd::connect_or_start()?;
    while let Ok(event) = frontend.events.recv() {
        match event {
            clientd::ServerEvent::Snapshot { services, .. } => {
                for service in services.into_iter().filter(|service| {
                    matches!(service.spec, Some(ServiceRequestKind::FolderSync { .. }))
                }) {
                    let endpoint = match service.spec.as_ref() {
                        Some(ServiceRequestKind::FolderSync {
                            remote_path,
                            local_path_hint,
                            mode,
                            ..
                        }) => format!(
                            "{} <-> {} ({mode:?})",
                            local_path_hint
                                .as_deref()
                                .unwrap_or("workstation directory"),
                            remote_path
                        ),
                        _ => unreachable!(),
                    };
                    println!(
                        "{}\t{:?}\t{}\t{}\t{}",
                        service.service_id,
                        service.status,
                        service.beam_uuid,
                        endpoint,
                        service.message.as_deref().unwrap_or("")
                    );
                }
                return Ok(0);
            }
            clientd::ServerEvent::Error { message } => bail!("{message}"),
            _ => {}
        }
    }
    bail!("local daemon disconnected before returning its service list")
}

fn control_folder_sync(service_id: &str, action: ServiceAction) -> anyhow::Result<u8> {
    let mut frontend = clientd::connect_or_start()?;
    frontend.control_service(service_id, action)?;
    eprintln!("beams: {action:?} requested for {service_id}");
    Ok(0)
}

fn copy_file(
    purpose: &str,
    login: &str,
    direction: FileCopyDirection,
    local_path: &str,
    remote_path: &str,
    overwrite: bool,
) -> anyhow::Result<u8> {
    let target = tracked_target(purpose, login)?;
    let mut frontend = clientd::connect_or_start()?;
    let request_id = frontend.create_file_copy(
        &target,
        direction,
        local_path,
        remote_path,
        if overwrite {
            OverwritePolicy::Replace
        } else {
            OverwritePolicy::Deny
        },
    )?;
    eprintln!("beams: submitted file-copy job {request_id}");
    while let Ok(event) = frontend.events.recv() {
        match event {
            clientd::ServerEvent::ServiceChanged { service }
                if service.request_id == request_id =>
            {
                if let Some(message) = &service.message {
                    eprintln!("beams: {:?}: {message}", service.status);
                }
                match service.status {
                    ServiceStatus::Stopped
                        if service.message.as_deref() == Some("cancelled locally") =>
                    {
                        return Ok(1);
                    }
                    ServiceStatus::Stopped => return Ok(0),
                    ServiceStatus::Error => return Ok(1),
                    _ => {}
                }
            }
            clientd::ServerEvent::Error { message } => bail!("{message}"),
            _ => {}
        }
    }
    bail!("local daemon disconnected before the file-copy job completed")
}

struct SshOptions {
    repo: PathBuf,
    purpose: String,
    region: Option<String>,
    login: String,
    profile: Option<String>,
    push_repo: bool,
    beam: Option<String>,
}

fn ssh(
    options: &SshOptions,
    escape_key: session::EscapeKey,
    display: session::DisplayOptions,
) -> anyhow::Result<u8> {
    let SshOptions { repo, purpose, region, login, .. } = options;
    let (purpose, login) = (purpose.as_str(), login.as_str());
    let profile = match &options.profile {
        Some(profile) => config::resolve_profile(profile, &std::env::current_dir()?),
        None => config::default_profile()?,
    };
    let mut state = state::State::load()?;

    let existing = match &options.beam {
        Some(target) => {
            let beamcmd::Located { endpoint, beam, .. } = beamcmd::locate(target, &mut state)?;
            if !has_ttl_remaining(&beam, 0) {
                bail!("beam {} has expired", beam.id);
            }
            // A launch recorded under any purpose tracking this Beam.
            let Some(launch) = state.purposes.values().filter(|e| e.uuid == beam.uuid).find_map(|e| e.launch.clone())
            else {
                bail!(
                    "beam {} has no profile launch recorded (an old repo-checkout Beam, or one this \
                     workstation didn't create); `tsh ssh {}@{}` still reaches it",
                    beam.id,
                    login,
                    beam.id
                );
            };
            Some((endpoint, beam, launch))
        }
        None => reusable_for_purpose(&mut state, purpose)?,
    };

    let (endpoint, beam, launch) = match existing {
        Some((endpoint, beam, launch)) => {
            let label = match &options.beam {
                Some(_) => "as requested".to_string(),
                None => format!("for purpose {purpose:?}"),
            };
            // `--beam` reattaches whatever it was launched with unless a
            // profile was asked for explicitly.
            if launch.profile != profile && (options.beam.is_none() || options.profile.is_some()) {
                bail!(
                    "beam {} ({label}) was launched with profile {}, not {profile}; use \
                     --purpose NAME for a new Beam, or remove this one with `tsh beams rm {}`",
                    beam.id,
                    launch.profile,
                    beam.id
                );
            }
            eprintln!(
                "beams: reusing beam {} ({}) {label} on {}, launch {} ({})",
                beam.id,
                beam.uuid,
                endpoint.label(),
                launch.launch_id,
                launch.profile
            );
            (endpoint, beam, launch)
        }
        None => {
            // New Beams go wherever the user is logged in now, and stay
            // pinned there.
            let endpoint = endpoint::Endpoint::current()?;
            let beams = tsh::list_beams(&endpoint)?;
            if beams.len() >= MAX_CONCURRENT_BEAMS {
                bail!(
                    "already at the {MAX_CONCURRENT_BEAMS}-beam limit ({} active) - clean one up \
                     with `tsh beams rm` first",
                    beams.len()
                );
            }

            // Pack before creating anything: a broken profile fails here.
            let (launch_dir, declaration) = config::pack_for_ssh(&profile)?;
            eprintln!("beams: packed launch {} ({profile})", declaration.launch_id);
            let packed = state::PurposeLaunch {
                launch_id: declaration.launch_id,
                profile: profile.clone(),
                plan_digest: declaration.plan_digest,
                launch_dir,
            };

            eprintln!(
                "beams: no usable beam for purpose {purpose:?}, spawning a new one on {}...",
                endpoint.label()
            );
            let fresh = tsh::add_beam(&endpoint, region.as_deref())?;

            eprintln!(
                "beams: waiting for {} to accept SSH connections...",
                fresh.id
            );
            tsh::wait_until_ready(&endpoint, &fresh.id, login, READY_TIMEOUT)?;

            if options.push_repo {
                eprintln!("beams: pushing {} to {}...", repo.display(), fresh.id);
                let branch = tsh::current_branch(repo)?;
                let origin_url = tsh::origin_url(repo)?;
                let beam = connection::TshConnection::new(
                    "tsh",
                    connection::BeamTarget {
                        beam_id: fresh.id.clone(),
                        beam_uuid: fresh.uuid.clone(),
                        login: login.to_string(),
                        endpoint: Some(endpoint.clone()),
                    },
                );
                tsh::prepare_git_target(
                    &beam,
                    REMOTE_REPO_PATH,
                    &branch,
                    origin_url.as_deref(),
                )?;
                tsh::push_repo(&beam, repo, REMOTE_REPO_PATH, &branch)?;
            }

            eprintln!("beams: copying the runtime and launch {} to {}...", packed.launch_id, fresh.id);
            let staging = stage::build_launch(repo, &packed.launch_dir, &packed.launch_id)?;
            let result = tsh::scp_dir(&endpoint, &staging.join(stage::LAUNCH_ROOT), &fresh.id);
            let _ = std::fs::remove_dir_all(&staging);
            result?;

            // Only now that the beam is actually usable - track it for
            // this purpose. Saving state before scp succeeds would leave
            // a purpose pointing at a beam that never got bootstrapped,
            // with nothing short of a `tsh beams rm` ever fixing it (the
            // TTL check would keep "reusing" it indefinitely).
            state.purposes.insert(
                purpose.to_string(),
                state::PurposeEntry {
                    beam_id: fresh.id.clone(),
                    uuid: fresh.uuid.clone(),
                    created_at: Utc::now().to_rfc3339(),
                    launch: Some(packed.clone()),
                    endpoint: Some(endpoint.clone()),
                },
            );
            state.save()?;

            (endpoint, fresh, packed)
        }
    };

    if let Some(instance) = clientd::instance()? {
        // Its own state.json: it only ever reaches Beams it created, so two
        // daemons never bridge (and contend for the agent relay of) one Beam.
        eprintln!("beams: using the {instance:?} instance (separate local daemon and Beams)");
    }

    let shell_command = format!(
        "{} launch start --launch-dir ~/{}/launches/{}",
        stage::RUNTIME_BIN,
        stage::LAUNCH_ROOT,
        launch.launch_id
    );
    eprintln!("beams: connecting to {} ({})...", beam.id, beam.uuid);
    // A launch needs the daemon: readiness and post work go over its bridge.
    let mut frontend = clientd::connect_or_start().context("the local service daemon is needed for a launch")?;
    let session_token = Uuid::new_v4().to_string();
    let target = connection::BeamTarget {
        beam_id: beam.id.clone(),
        beam_uuid: beam.uuid.clone(),
        login: login.to_string(),
        endpoint: Some(endpoint.clone()),
    };
    // Before the bridge, so the daemon knows the launch by the time it connects.
    frontend.start_launch(&target, &launch.launch_dir, beams_protocol::launch::Gate::Interactive)?;
    frontend.ensure_bridge(&target, &session_token)?;
    let remote_command =
        format!("env BEAMS_CLIENT_ID={} BEAMS_SESSION_TOKEN={session_token} {shell_command}", frontend.client_id);
    let local_daemon = session::LocalDaemon::connected(frontend, target, session_token);
    tsh::ssh_interactive(
        &endpoint,
        login,
        &beam.id,
        &beam.uuid,
        &[remote_command],
        escape_key,
        local_daemon,
        display,
    )
}

/// Does `beam` have more than `hours` of TTL left? Beams can't have their
/// TTL extended (project.md), so this is the only "is it still worth
/// reusing" signal available.
fn has_ttl_remaining(beam: &tsh::BeamInfo, hours: i64) -> bool {
    match DateTime::parse_from_rfc3339(&beam.expires) {
        Ok(expires) => expires.signed_duration_since(Utc::now()) > Duration::hours(hours),
        Err(_) => false,
    }
}

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(rest)
        }
        None => PathBuf::from(path),
    }
}
