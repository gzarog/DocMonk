//! The background daemon's lifecycle.
//!
//! `start` spawns `ragmonk daemon run` detached, logging to
//! `logs/daemon.out.log`, records its PID and waits for its first health
//! snapshot. `stop` signals it and waits. Status comes from the PID and
//! health files; nothing talks to the daemon process directly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use docmonk_core::errors::RagMonkError;
use docmonk_core::paths::Home;
use docmonk_indexing::coordinator::Options;
use docmonk_indexing::coordinator::{Progress, Registry, SourceResult};
use docmonk_indexing::daemon::{
    health, pid, Catalog, CoordinatorRunner, Daemon, DaemonOptions, PassRunner, ScanRequest,
};
use docmonk_indexing::runtime::is_process_alive;
use docmonk_storage::control::ControlPlane;
use docmonk_storage::control::SourceRecord;
use docmonk_storage::StorageLayout;

use crate::{generic, load};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(100);

/// The result of [`start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    AlreadyRunning(i64),
    Started(i64),
}

/// The result of [`stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    NotRunning,
    Stopped(i64),
}

/// Spawns this executable's `daemon run` in its own process group.
fn spawn(home: &Home) -> Result<u32, RagMonkError> {
    let log_path = home.logs_dir().join("daemon.out.log");
    std::fs::create_dir_all(home.logs_dir()).map_err(generic)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(generic)?;
    let mut cmd = std::process::Command::new(std::env::current_exe().map_err(generic)?);
    cmd.args(["daemon", "run"])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(generic)?)
        .stderr(log);
    #[cfg(unix)]
    {
        // Own process group: survives the caller exiting and a Ctrl+C sent
        // to the shell's group.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    Ok(cmd.spawn().map_err(generic)?.id())
}

/// Starts the daemon unless one is running, and waits until it is healthy.
pub fn start(home: &Home) -> Result<Started, RagMonkError> {
    if let Some(existing) = pid::running_daemon(home) {
        return Ok(Started::AlreadyRunning(existing.pid));
    }
    // A stale snapshot from an earlier run must not count as "healthy".
    let _ = std::fs::remove_file(home.daemon_health());
    let child = i64::from(spawn(home)?);
    pid::write_pid_file(home, child).map_err(generic)?;
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if !is_process_alive(child) {
            break;
        }
        if health::read_health(home).is_some() {
            return Ok(Started::Started(child));
        }
        std::thread::sleep(POLL);
    }
    pid::remove_pid_file(home);
    Err(RagMonkError::usage(format!(
        "daemon did not report healthy within {:.0}s; see {}",
        START_TIMEOUT.as_secs_f64(),
        home.logs_dir().join("daemon.out.log").display()
    )))
}

/// Stops the running daemon and waits for it to exit.
pub fn stop(home: &Home) -> Result<Stopped, RagMonkError> {
    let Some(info) = pid::running_daemon(home) else {
        pid::remove_pid_file(home);
        return Ok(Stopped::NotRunning);
    };
    pid::signal_stop(info.pid);
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline && is_process_alive(info.pid) {
        std::thread::sleep(POLL);
    }
    if is_process_alive(info.pid) {
        return Err(RagMonkError::usage(format!(
            "daemon (pid {}) did not stop within {:.0}s",
            info.pid,
            STOP_TIMEOUT.as_secs_f64()
        )));
    }
    pid::remove_pid_file(home);
    Ok(Stopped::Stopped(info.pid))
}

fn stop_flag() -> Result<Arc<AtomicBool>, RagMonkError> {
    let flag = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(sig, Arc::clone(&flag)).map_err(generic)?;
    }
    Ok(flag)
}

/// Runs the daemon in this process until SIGTERM/SIGINT. `on_ready` is
/// called with this process's PID once the daemon is running.
pub fn run_foreground(home: &Home, on_ready: impl FnOnce(i64)) -> Result<(), RagMonkError> {
    let me = i64::from(std::process::id());
    if let Some(other) = pid::running_daemon(home).filter(|i| i.pid != me) {
        return Err(RagMonkError::usage(format!(
            "daemon already running (pid {})",
            other.pid
        )));
    }
    // `start` records the child's PID itself. A bare foreground run
    // records its own so that `status` and `stop` find it.
    if pid::read_pid_file(home).is_none_or(|i| i.pid != me) {
        pid::write_pid_file(home, me).map_err(generic)?;
    }
    let cfg = load(home)?;
    let _ = docmonk_telemetry::logging::configure_logging(
        &home.logs_dir(),
        &cfg.runtime.log_level,
        "text",
    );
    let stop = stop_flag()?;
    docmonk_indexing::governor::configure(docmonk_indexing::governor::Limits::from_config(
        &cfg.indexing,
    ));
    let workers = cfg.indexing.resolved_max_parallel_sources().max(1);
    let registry =
        || docmonk_convert::registry_with(&cfg, &docmonk_convert::RegistryOptions::for_home(home));
    let (catalog, runners): (Box<dyn Catalog>, Vec<Box<dyn PassRunner>>) =
        match crate::backend::open_for_write(home)? {
            crate::backend::Backend::Local => {
                let layout = StorageLayout::new(home);
                let cache = cfg.runtime.sqlite_cache_size_mb;
                let open = || ControlPlane::open(&layout, cache).map_err(generic);
                // The daemon's catalog connection, and each worker's own.
                let mut runners: Vec<Box<dyn PassRunner>> = Vec::new();
                let graph: docmonk_indexing::daemon::GraphHook = {
                    let (home, cfg) = (home.clone(), cfg.clone());
                    Arc::new(move |source: &SourceRecord| {
                        Some(daemon_graph(
                            &home,
                            &cfg,
                            &crate::backend::Backend::Local,
                            source,
                        ))
                    })
                };
                for _ in 0..workers {
                    runners.push(Box::new(CoordinatorRunner {
                        layout: layout.clone(),
                        control: open()?,
                        registry: registry(),
                        opts: Options::from_config(&cfg),
                        graph: Some(graph.clone()),
                    }));
                }
                (Box::new(open()?), runners)
            }
            crate::backend::Backend::Server(server) => {
                let mut runners: Vec<Box<dyn PassRunner>> = Vec::new();
                for _ in 0..workers {
                    runners.push(Box::new(ServerRunner {
                        home: home.clone(),
                        cfg: cfg.clone(),
                        server: server.clone(),
                        registry: registry(),
                        opts: Options::from_config(&cfg),
                    }));
                }
                (Box::new(ServerCatalog(server)), runners)
            }
        };
    let daemon = Daemon::start_with_runners(
        home.clone(),
        DaemonOptions::from_config(&cfg),
        catalog,
        runners,
    )?;
    on_ready(me);
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }
    daemon.stop();
    if pid::read_pid_file(home).is_some_and(|i| i.pid == me) {
        pid::remove_pid_file(home);
    }
    Ok(())
}

/// The server catalog as the daemon's source list.
struct ServerCatalog(Arc<docmonk_backends::ServerBackend>);

impl Catalog for ServerCatalog {
    fn list_sources(&mut self, enabled_only: bool) -> Result<Vec<SourceRecord>, RagMonkError> {
        crate::sources::Catalog::Server(self.0.clone()).list(enabled_only)
    }
    fn get_source(&mut self, id: &str) -> Result<Option<SourceRecord>, RagMonkError> {
        Ok(self
            .0
            .catalog_entry(id)
            .map_err(crate::backend::server_err)?
            .map(|e| crate::backend::record_of(&e)))
    }
}

/// Server-mode daemon passes: the staged pipeline plus server publication
/// (see [`crate::server_index`]).
struct ServerRunner {
    home: docmonk_core::paths::Home,
    cfg: docmonk_config::RagMonkConfig,
    server: Arc<docmonk_backends::ServerBackend>,
    registry: Registry,
    opts: Options,
}

impl PassRunner for ServerRunner {
    fn run_pass(
        &mut self,
        source: &SourceRecord,
        request: &ScanRequest,
        progress: &mut dyn Progress,
    ) -> Result<SourceResult, RagMonkError> {
        crate::server_index::run_source(
            &self.home,
            &self.cfg,
            &self.server,
            source,
            &self.registry,
            &self.opts,
            &crate::indexing::RunOptions {
                targets: (!request.full).then(|| request.changed_paths.clone()),
                ..crate::indexing::RunOptions::default()
            },
            progress,
        )
    }

    fn run_graph(&mut self, source: &SourceRecord) -> Option<String> {
        Some(daemon_graph(
            &self.home,
            &self.cfg,
            &crate::backend::Backend::Server(self.server.clone()),
            source,
        ))
    }
}

/// A daemon pass's relationship graph stage (the caller holds the source's
/// lock): built when enabled, marked disabled otherwise.
fn daemon_graph(
    home: &docmonk_core::paths::Home,
    cfg: &docmonk_config::RagMonkConfig,
    backend: &crate::backend::Backend,
    source: &SourceRecord,
) -> String {
    if !cfg.indexing.relationships_enabled {
        if let Err(e) = crate::relationships::mark_disabled(home, cfg, backend, source) {
            tracing::debug!(component = "daemon", event = "disable_not_recorded", source_id = %source.id, error = %e.message());
        }
        return "disabled".into();
    }
    let outcome = crate::relationships::graph_one(
        home,
        cfg,
        backend,
        source,
        &Options::from_config(cfg),
        None,
    );
    if let Some(e) = outcome.error() {
        tracing::warn!(component = "daemon", event = "daemon_graph_not_published", source_id = %source.id, state = outcome.state(), error = %e);
    }
    // Consumers impacted by this source (and this source as a consumer).
    // A consumer that cannot be updated now stays marked stale in its
    // manifest and is retried by the next pass.
    if let Ok(all) = crate::sources::catalog(home).and_then(|c| c.list(false)) {
        crate::dependencies::run_stage_holding(
            home,
            cfg,
            backend,
            &all,
            Options::from_config(cfg).lock_timeout,
            Some(&source.id),
            |consumer, o| {
                tracing::debug!(component = "daemon", event = "dependencies", source_id = %consumer.id, state = o.state());
            },
        );
    }
    outcome.state().into()
}
