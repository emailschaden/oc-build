#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
#![deny(clippy::indexing_slicing)]
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
#[cfg(all(feature = "jemalloc", feature = "release-dist", unix))]
mod jemalloc_malloc_conf {
    /// jemalloc looks up `extern const char *malloc_conf`, a thin pointer, not a Rust `&[u8]` fat pointer.
    #[repr(transparent)]
    struct MallocConfPtr(*const u8);
    unsafe impl Sync for MallocConfPtr {}
    static CONF: [u8; 63] = *b"prof:true,prof_active:false,lg_prof_sample:19,prof_final:false\0";
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    #[used]
    #[unsafe(export_name = "malloc_conf")]
    static MALLOC_CONF: MallocConfPtr = MallocConfPtr(CONF.as_ptr());
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[used]
    #[unsafe(export_name = "_rjem_malloc_conf")]
    static MALLOC_CONF: MallocConfPtr = MallocConfPtr(CONF.as_ptr());
}
use anyhow::Result;
use std::env;
use std::io::Write;
use std::num::NonZeroUsize;
use xai_grok_pager::app::{Command, PagerArgs};
// oc-build: opencode backend; no update channel, the label is always empty.
const CHANNEL_LABEL: &str = "";
/// Entrypoint tag for `grok -p`; keys the quiet stderr default in `init_tracing_simple`.
const HEADLESS_ENTRYPOINT: &str = "headless";
/// Initialize simple tracing for non-TUI agent modes.
fn init_tracing_simple(app_entrypoint: &'static str) {
    use tracing_subscriber::{EnvFilter, Layer as _, fmt, layer::SubscriberExt as _};
    use xai_grok_telemetry::debug_log::RMCP_SSE_NOISE_TARGET;
    let default_filter = if app_entrypoint == HEADLESS_ENTRYPOINT {
        "off"
    } else {
        "error"
    };
    let env_filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter.add_directive(
            format!("{RMCP_SSE_NOISE_TARGET}=error")
                .parse()
                .expect("static rmcp directive must parse"),
        ),
        Err(_) => EnvFilter::new(default_filter),
    };
    let fmt_layer = fmt::layer()
        .with_target(false)
        .with_ansi(true)
        .with_writer(std::io::stderr);
    let registry = tracing_subscriber::registry()
        .with(fmt_layer.with_filter(env_filter))
        .with(xai_grok_telemetry::sampling_log::layer())
        .with(xai_grok_telemetry::span_profile::layer(app_entrypoint))
        .with(xai_grok_telemetry::instrumentation::layer())
        .with(xai_grok_telemetry::hooks_log::layer());
    // oc-build: opencode backend; local logging only (no OTLP or external exporter).
    xai_grok_telemetry::debug_log::install_firehose(registry, app_entrypoint);
}
/// Flush the local debug log, then exit.
fn shutdown_and_flush_telemetry(exit_code: i32) -> ! {
    xai_grok_telemetry::debug_log::flush();
    finalize_span_profile();
    std::process::exit(exit_code);
}
fn finalize_span_profile() {
    if let Some(path) = xai_grok_telemetry::span_profile::finalize() {
        eprintln!("grok: span profile written to {}", path.display());
    }
}
/// Raise the per-process fd soft limit toward the hard limit. A 1024 limit fails with EMFILE under a ~100-session
/// wave. Best-effort: never blocks startup (containers/cgroups may pin limits).
#[cfg(unix)]
fn raise_fd_limit() {
    #[cfg(target_os = "macos")]
    const TARGET: libc::rlim_t = 8192;
    #[cfg(not(target_os = "macos"))]
    const TARGET: libc::rlim_t = 65536;
    unsafe {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            return;
        }
        let new_cur = rlim.rlim_max.min(TARGET);
        if new_cur <= rlim.rlim_cur {
            return;
        }
        let old = rlim.rlim_cur;
        rlim.rlim_cur = new_cur;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0 {
            tracing::trace!(old, new = new_cur, "raised RLIMIT_NOFILE");
        }
    }
}
#[cfg(not(unix))]
fn raise_fd_limit() {}
/// Clears `args.command` so the regular subcommand match doesn't try to handle it. The only gate is the feature
/// flag: a disabled dashboard (`[dashboard].enabled = false` / `GROK_AGENT_DASHBOARD=0`) is a CLI error. It fires
/// here, before the TUI starts, because the welcome view silently drops the equivalent runtime toast.
fn flag_dashboard_at_startup_if_requested(args: &mut PagerArgs) -> Result<()> {
    if !matches!(args.command, Some(Command::Dashboard)) {
        return Ok(());
    }
    if !xai_grok_pager::views::dashboard::dashboard_enabled() {
        anyhow::bail!(
            "the Agent Dashboard is disabled. Enable it by removing \
             `[dashboard] enabled = false` from ~/.grok/config.toml and \
             unsetting GROK_AGENT_DASHBOARD=0."
        );
    }
    args.command = None;
    unsafe { std::env::set_var("GROK_OPEN_DASHBOARD_AT_STARTUP", "1") };
    Ok(())
}
/// Kick off background work that overlaps startup. Add new prewarms here.
fn schedule_startup_prewarm() {
    xai_grok_shell::agent::mvp_agent::warm_async_http_client();
}
fn configure_process_env(mut args: PagerArgs) -> Result<PagerArgs> {
    flag_dashboard_at_startup_if_requested(&mut args)?;
    let args = args.apply_cwd()?;
    unsafe {
        if let Some(mode) = args.compaction_mode.as_deref() {
            std::env::set_var("GROK_COMPACTION_MODE", mode);
        }
        if let Some(detail) = args.compaction_detail.as_deref() {
            std::env::set_var("GROK_COMPACTION_DETAIL", detail);
        }
        if args.chat() {
            std::env::set_var(xai_grok_shell::agent::chat_modes::GROK_CHAT_MODE_ENV, "1");
        }
        if args.log_sampling {
            std::env::set_var("GROK_LOG_SAMPLING", "1");
        }
        if let Some(path) = args.debug_file.as_deref() {
            std::env::set_var("GROK_DEBUG_LOG", path);
            std::env::remove_var("GROK_LOG_FILE");
        }
        if args.debug || args.debug_file.is_some() {
            if std::env::var_os("GROK_DEBUG_LOG").is_none() {
                std::env::set_var("GROK_DEBUG_LOG", "1");
            }
            if std::env::var_os("GROK_HOOKS_LOG").is_none() {
                std::env::set_var("GROK_HOOKS_LOG", "1");
            }
        }
    }
    Ok(args)
}
const RUNTIME_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
const GROK_WORKER_THREADS_ENV: &str = "GROK_WORKER_THREADS";
/// tokio defaults to one worker per logical CPU.
/// On a host with hundreds of CPUs that can exhaust a cgroup thread budget at startup and abort under `panic = "abort"`.
/// A terminal UI is I/O-bound, so cap at 8.
const DEFAULT_MAX_WORKER_THREADS: NonZeroUsize = NonZeroUsize::new(8).unwrap();
/// How `GROK_WORKER_THREADS` resolved.
#[derive(Debug, PartialEq, Eq)]
enum WorkerCount {
    Accepted(NonZeroUsize),
    Clamped {
        requested: i128,
        used: NonZeroUsize,
        cores: NonZeroUsize,
    },
    Ignored {
        value: String,
        used: NonZeroUsize,
    },
}
impl WorkerCount {
    fn used(&self) -> NonZeroUsize {
        match self {
            Self::Accepted(used) | Self::Clamped { used, .. } | Self::Ignored { used, .. } => *used,
        }
    }
    fn notice(&self) -> Option<String> {
        match self {
            Self::Accepted(_) => None,
            Self::Clamped {
                requested,
                used,
                cores,
            } => Some(format!(
                "grok: clamped {GROK_WORKER_THREADS_ENV}={requested} to {used} (valid range is 1..={cores})"
            )),
            Self::Ignored { value, .. } => Some(format!(
                "grok: ignoring {GROK_WORKER_THREADS_ENV}={value:?} (not a valid integer)"
            )),
        }
    }
}
fn cli_worker_threads() -> NonZeroUsize {
    let cores = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
    let resolved = match std::env::var(GROK_WORKER_THREADS_ENV) {
        Ok(value) => worker_threads_from(Some(&value), cores),
        Err(std::env::VarError::NotPresent) => worker_threads_from(None, cores),
        Err(std::env::VarError::NotUnicode(value)) => WorkerCount::Ignored {
            value: value.to_string_lossy().into_owned(),
            used: default_worker_threads(cores),
        },
    };
    if let Some(notice) = resolved.notice() {
        eprintln!("{notice}");
    }
    resolved.used()
}
fn worker_threads_from(env_override: Option<&str>, cores: NonZeroUsize) -> WorkerCount {
    match env_override {
        Some(value) => resolve_worker_override(value, cores),
        None => WorkerCount::Accepted(default_worker_threads(cores)),
    }
}
fn default_worker_threads(cores: NonZeroUsize) -> NonZeroUsize {
    cores.min(DEFAULT_MAX_WORKER_THREADS)
}
fn resolve_worker_override(value: &str, cores: NonZeroUsize) -> WorkerCount {
    let Ok(requested) = value.trim().parse::<i128>() else {
        return WorkerCount::Ignored {
            value: value.to_owned(),
            used: default_worker_threads(cores),
        };
    };
    let clamped = requested.clamp(1, cores.get() as i128) as usize;
    let used = NonZeroUsize::new(clamped).expect("clamp floor of 1 guarantees non-zero");
    if requested == used.get() as i128 {
        WorkerCount::Accepted(used)
    } else {
        WorkerCount::Clamped {
            requested,
            used,
            cores,
        }
    }
}
/// A plain runtime drop blocks forever on an uncancellable in-flight blocking task.
/// `shutdown_timeout` abandons it after `grace` so exit can't hang.
fn run_and_shutdown<F: std::future::Future>(
    runtime: tokio::runtime::Runtime,
    fut: F,
    grace: std::time::Duration,
) -> F::Output {
    let output = runtime.block_on(fut);
    runtime.shutdown_timeout(grace);
    output
}
/// Return freed-but-retained jemalloc pages to the OS. Resuming a long session then doesn't leave hundreds of MB of
/// dead pages counted against the process for its lifetime.
#[cfg(all(feature = "jemalloc", unix))]
fn purge_jemalloc_retained_pages() {
    static NAME: &[u8] = b"arena.4096.purge\0";
    let ret = unsafe {
        tikv_jemalloc_sys::mallctl(
            NAME.as_ptr().cast(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 {
        static WARN_ONCE: std::sync::Once = std::sync::Once::new();
        WARN_ONCE.call_once(|| {
            tracing::warn!(
                errno = ret,
                "jemalloc arena purge mallctl failed; retained-page release is inert"
            );
        });
    }
}
/// Allocator gauges for the memory trace (`memory_trace` hook): advance the jemalloc epoch so the `stats.*` reads are current, then read each gauge.
/// Returns `None` if any mallctl fails (trace records the absence).
/// Uses the `tikv-jemalloc-ctl` raw helpers (shared with the heap-profile hooks below) instead of hand-rolled mallctl.
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_allocator_stats() -> Option<xai_grok_pager::memory_trace::AllocatorStats> {
    /// SAFETY: callers pass fixed NUL-terminated `stats.*` size_t ctl names.
    unsafe fn gauge(name: &[u8]) -> Option<u64> {
        unsafe {
            tikv_jemalloc_ctl::raw::read::<usize>(name)
                .ok()
                .map(|v| v as u64)
        }
    }
    unsafe {
        tikv_jemalloc_ctl::raw::write(b"epoch\0", 1u64).ok()?;
        Some(xai_grok_pager::memory_trace::AllocatorStats {
            allocated: gauge(b"stats.allocated\0")?,
            active: gauge(b"stats.active\0")?,
            resident: gauge(b"stats.resident\0")?,
            mapped: gauge(b"stats.mapped\0")?,
            retained: gauge(b"stats.retained\0")?,
            metadata: gauge(b"stats.metadata\0")?,
        })
    }
}
/// Full jemalloc statistics dump for threshold snapshots (`malloc_stats_print` default human-readable format, arena detail included).
/// This is the artifact the GCS memory-trace upload ships for offline analysis.
/// Raw `tikv_jemalloc_sys` because jemalloc-ctl has no callback-form stats_print.
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_stats_dump() -> String {
    unsafe extern "C" fn append(opaque: *mut std::ffi::c_void, msg: *const std::ffi::c_char) {
        unsafe {
            let out = &mut *opaque.cast::<String>();
            out.push_str(&std::ffi::CStr::from_ptr(msg).to_string_lossy());
        }
    }
    let mut out = String::new();
    unsafe {
        tikv_jemalloc_sys::malloc_stats_print(
            Some(append),
            (&raw mut out).cast(),
            std::ptr::null(),
        );
    }
    out
}
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_heap_stats() -> Option<xai_grok_shell::heap_profile::JemallocStats> {
    unsafe {
        tikv_jemalloc_ctl::raw::write(b"epoch\0", 1u64).ok()?;
        let allocated = tikv_jemalloc_ctl::raw::read::<usize>(b"stats.allocated\0").ok()? as u64;
        let resident = tikv_jemalloc_ctl::raw::read::<usize>(b"stats.resident\0").ok()? as u64;
        Some(xai_grok_shell::heap_profile::JemallocStats {
            allocated,
            resident,
        })
    }
}
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_set_prof_active(active: bool) -> bool {
    unsafe { tikv_jemalloc_ctl::raw::write(b"prof.active\0", active).is_ok() }
}
#[cfg(all(test, feature = "jemalloc", unix))]
fn jemalloc_read_prof_active() -> Option<bool> {
    unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"prof.active\0").ok() }
}
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_prof_available() -> bool {
    unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"opt.prof\0").unwrap_or(false) }
}
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_dump_to_path(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    if !jemalloc_prof_available() {
        return Err("opt.prof false".into());
    }
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    unsafe { tikv_jemalloc_ctl::raw::write(b"prof.dump\0", c.as_ptr()) }.map_err(|e| e.to_string())
}
#[cfg(all(feature = "jemalloc", unix))]
fn install_heap_profile_hooks() {
    xai_grok_shell::heap_profile::install(xai_grok_shell::heap_profile::HeapProfileHooks {
        stats: jemalloc_heap_stats,
        set_prof_active: jemalloc_set_prof_active,
        dump_to_path: jemalloc_dump_to_path,
        prof_available: jemalloc_prof_available,
    });
}
fn version_text(channel_label: &str) -> String {
    format!(
        "grok {}\n",
        xai_grok_version::display_version_with_commit(
            xai_grok_version::full_version(),
            channel_label,
        )
    )
}
fn write_version(writer: &mut impl std::io::Write, channel_label: &str) -> std::io::Result<()> {
    writer.write_all(version_text(channel_label).as_bytes())
}
fn crash_dir_for(_args: &PagerArgs) -> std::path::PathBuf {
    xai_grok_shell::util::grok_home::grok_home().join("crash")
}
fn dispatch_version_if_requested(args: &PagerArgs) -> bool {
    if !args.version {
        return false;
    }
    if let Err(error) = write_version(&mut std::io::stdout().lock(), CHANNEL_LABEL) {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
    true
}
fn dispatch_doctor_if_requested(args: &PagerArgs) -> bool {
    let Some(Command::Doctor(doctor_args)) = &args.command else {
        return false;
    };
    if let Err(error) = xai_grok_pager::doctor_cmd::run(doctor_args.clone()) {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
    true
}
fn main() {
    xai_grok_version::set_full_version(env!("VERSION_WITH_COMMIT"));
    xai_grok_telemetry::startup::mark_process_start();
    if let Some(code) = xai_grok_pager::app::mermaid_worker::maybe_run_render_subprocess() {
        std::process::exit(code);
    }
    if let Some(code) = xai_grok_pager::voice::maybe_run_capture_subprocess() {
        std::process::exit(code);
    }
    let args = PagerArgs::parse_cli();
    if dispatch_version_if_requested(&args) || dispatch_doctor_if_requested(&args) {
        return;
    }
    xai_grok_pager_minimal::install();
    #[cfg(all(feature = "jemalloc", unix))]
    xai_grok_pager::memory_release::install_release_hook(purge_jemalloc_retained_pages);
    #[cfg(all(feature = "jemalloc", unix))]
    {
        xai_grok_pager::memory_trace::install_allocator_stats_provider(jemalloc_allocator_stats);
        xai_grok_pager::memory_trace::install_allocator_dump_provider(jemalloc_stats_dump);
    }
    #[cfg(all(feature = "jemalloc", unix))]
    install_heap_profile_hooks();
    let args = configure_process_env(args).unwrap_or_else(|err| {
        eprintln!("grok: {err:#}");
        std::process::exit(1);
    });
    xai_grok_pager::memory_trace::start(xai_grok_pager::memory_trace::default_dir());
    raise_fd_limit();
    xai_grok_pager::docs::extract_user_guide_docs(&xai_grok_shell::util::grok_home::grok_home());
    xai_crash_handler::install_terminal_restore_only();
    if xai_grok_shell::util::config::load_crash_handler_enabled_sync() {
        let crash_dir = crash_dir_for(&args);
        if let Some(report) = xai_crash_handler::check_previous_crash(&crash_dir) {
            eprintln!("Grok crashed during your last session.");
            eprintln!("  Signal:  {}", report.signal_name);
            eprintln!("  Version: {}", report.app_version);
            eprintln!("  Report:  {}", report.report_path.display());
            eprintln!();
        }
        if !xai_crash_handler::install(xai_crash_handler::CrashHandlerConfig {
            app_version: env!("VERSION_WITH_COMMIT").to_string(),
            crash_dir: crash_dir.clone(),
        }) {
            eprintln!(
                "warning: crash handler enabled but failed to install (check permissions on {})",
                crash_dir.display()
            );
        }
    }
    let workers = cli_worker_threads();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(workers.get()).enable_all();
    let runtime =
        xai_tty_utils::runtime::build_with_blocking_pool(&mut builder).unwrap_or_else(|e| {
            eprintln!("grok: failed to start tokio runtime: {e}");
            shutdown_and_flush_telemetry(1);
        });
    let result = run_and_shutdown(runtime, async_main(args), RUNTIME_SHUTDOWN_GRACE);
    xai_grok_telemetry::debug_log::flush();
    if let Err(e) = result {
        xai_tty_utils::restore_native_stderr();
        finalize_span_profile();
        let report = match e.downcast_ref::<xai_grok_pager::app::StartupFailure>() {
            Some(startup) => startup.user_report(),
            None => format!("Error: {e:#}"),
        };
        xai_grok_pager::best_effort_stderr::eprint_line(&report);
        std::process::exit(1);
    }
    finalize_span_profile();
}
#[tracing::instrument(level = "debug", skip_all)]
async fn async_main(mut args: PagerArgs) -> Result<()> {
    xai_grok_extra_ca::ensure_default_crypto_provider();
    if let Some(Command::Completions { shell }) = &args.command {
        xai_grok_pager::completions_cmd::run(*shell);
        return Ok(());
    }
    if let Some(Command::Wrap(ref wrap_args)) = args.command {
        return xai_grok_pager::wrap_cmd::run(wrap_args);
    }
    let is_interactive = args.command.is_none()
        && args.single.is_none()
        && args.prompt_json.is_none()
        && args.prompt_file.is_none();
    xai_grok_shell::http::set_client_name(if is_interactive {
        xai_grok_workspace::permission::ClientType::GrokPager
    } else {
        xai_grok_workspace::permission::ClientType::Generic
    });
    schedule_startup_prewarm();
    args.pin_local_resume_target()?;
    let saved_profile = args.saved_resume_profile();
    let sandbox_profile_arg = match args.startup_sandbox_profile(saved_profile.as_deref()) {
        xai_grok_pager::app::cli::SandboxStartup::Apply(profile) => profile,
        xai_grok_pager::app::cli::SandboxStartup::Conflict { requested, saved } => {
            eprintln!(
                "error: cannot resume this session under sandbox profile '{requested}' — \
                 it was created with '{saved}'. Omit --sandbox to resume with '{saved}', \
                 or start a new session to use '{requested}'."
            );
            std::process::exit(1);
        }
    };
    if args.trust {
        use xai_grok_workspace::folder_trust::{grant_folder_trust, report_cli_trust_grant};
        match std::env::current_dir() {
            Ok(cwd) => report_cli_trust_grant(&grant_folder_trust(&cwd)),
            Err(e) => {
                tracing::warn!(error = %e, "--trust: failed to resolve cwd; folder not trusted");
                let _ = writeln!(
                    std::io::stderr(),
                    "error: --trust: failed to resolve cwd; folder not trusted: {e}"
                );
            }
        }
    }
    xai_grok_shell::config::apply_sandbox(
        None,
        sandbox_profile_arg.as_deref(),
        args.cwd.as_deref(),
    );
    if let Some(command) = args.command.take() {
        match command {
            Command::Version { json } => {
                if json {
                    let payload = serde_json::json!({
                        "currentVersion": env!("VERSION_WITH_COMMIT"),
                    });
                    println!("{}", serde_json::to_string(&payload)?);
                } else {
                    write_version(&mut std::io::stdout().lock(), CHANNEL_LABEL)?;
                }
                return Ok(());
            }
            Command::Doctor(_) => {
                unreachable!("doctor was consumed before runtime startup")
            }
            Command::Inspect { json } => {
                let cwd = std::env::current_dir().unwrap_or_default();
                xai_grok_shell::inspect::inspect(&cwd, json).await?;
                return Ok(());
            }
            Command::Mcp(mcp_args) => {
                init_tracing_simple("cli");
                return xai_grok_pager::mcp_cmd::run(mcp_args).await;
            }
            Command::Plugin(plugin_args) => {
                init_tracing_simple("cli");
                return xai_grok_pager::plugin_cmd::run(plugin_args).await;
            }
            Command::Models => {
                init_tracing_simple("cli");
                // oc-build: opencode backend
                let raw_config = xai_grok_shell::config::load_effective_config()?;
                return xai_grok_pager::models::list_available_models(
                    &args.opencode_config(&raw_config),
                )
                .await;
            }
            Command::DiskUsage(disk_usage_args) => {
                init_tracing_simple("cli");
                return xai_grok_pager::disk_usage_cmd::run(disk_usage_args);
            }
            Command::Sessions(sessions_args) => {
                init_tracing_simple("cli");
                let agent_config = xai_grok_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                return xai_grok_pager::sessions_cmd::run(sessions_args, &agent_config).await;
            }
            Command::Export(export_args) => {
                init_tracing_simple("cli");
                return xai_grok_pager::export_cmd::run(export_args);
            }
            Command::Memory(memory_args) => {
                let config = xai_grok_shell::config::load_effective_config()?;
                let mode = xai_grok_shell::config::MemoryConfig::resolve(
                    args.experimental_memory,
                    args.no_memory,
                    &config,
                    None,
                )
                .mode;
                return xai_grok_pager::memory_cmd::run(memory_args, mode);
            }
            Command::Wrap(ref wrap_args) => {
                return xai_grok_pager::wrap_cmd::run(wrap_args);
            }
            Command::Completions { shell } => {
                xai_grok_pager::completions_cmd::run(shell);
                return Ok(());
            }
            Command::Dashboard => {
                args.command = Some(Command::Dashboard);
                flag_dashboard_at_startup_if_requested(&mut args)?;
            }
        }
    }
    let headless_prompt = xai_grok_pager::headless::HeadlessPrompt::from_args(
        args.single.as_deref(),
        args.prompt_json.as_deref(),
        args.prompt_file.as_deref(),
    )?;
    if headless_prompt.is_some() || args.memory_flush {
        if args.memory_flush
            && headless_prompt.is_none()
            && args.resume_session.is_none()
            && args.load_session.is_none()
            && !args.continue_last_session
        {
            anyhow::bail!("--memory-flush without a prompt requires --resume/-r or --continue/-c");
        }
        init_tracing_simple(HEADLESS_ENTRYPOINT);
        let launch_yolo = xai_grok_shell::util::config::effective_yolo_for_launch(
            args.yolo,
            args.permission_mode_flag.as_deref(),
            None,
        );
        if let Some(warning) = launch_yolo.blocked_warning {
            eprintln!("grok: {warning}");
        }
        let json_schema = args
            .json_schema
            .as_deref()
            .map(xai_grok_pager::headless::parse_json_schema)
            .transpose()?;
        if json_schema.is_some()
            && args.output_format == xai_grok_pager::headless::OutputFormat::Plain
        {
            args.output_format = xai_grok_pager::headless::OutputFormat::Json;
        }
        let memory_enabled_override = args.memory_enabled_override();
        let memory_flush = args.memory_flush;
        // oc-build: opencode backend
        let opencode = args.opencode_config(&xai_grok_shell::config::load_effective_config()?);
        return xai_grok_pager::headless::run_single_turn(
            headless_prompt,
            args.verbatim,
            xai_grok_pager::headless::HeadlessOptions {
                opencode,
                session_id: args.session_id.clone(),
                resume: args.resume_session.or(args.load_session),
                resume_title_pinned: args.resume_target_pinned,
                cwd: args.cwd,
                yolo: launch_yolo.yolo,
                trust: args.trust,
                output_format: args.output_format,
                include_partial_messages: args.include_partial_messages,
                json_schema,
                model: args.model,
                rules: args.rules,
                system_prompt_override: args.system_prompt_override.clone(),
                continue_last_session: args.continue_last_session,
                fork_session: args.fork_session,
                worktree: args.worktree,
                worktree_ref: args.worktree_ref,
                restore_code: args.restore_code,
                agent: args.agent.clone(),
                agents_json: args.agents_json.clone(),
                cli_tools: args.cli_tools.clone(),
                cli_disallowed_tools: args.cli_disallowed_tools.clone(),
                disable_web_search: args.disable_web_search,
                allow_rules: args.allow_rules.clone(),
                deny_rules: args.deny_rules.clone(),
                max_turns: args.max_turns,
                permission_mode_flag: args.permission_mode_flag.clone(),
                reasoning_effort: args.reasoning_effort.clone(),
                wait_for_background: !args.no_wait_for_background,
                background_wait_timeout: std::time::Duration::from_secs(
                    args.background_wait_timeout_secs,
                ),
                memory_flush,
                memory_enabled_override,
            },
        )
        .await;
    }
    // oc-build: opencode backend; no update check or auto-update on exit.
    let result = xai_grok_pager::app::run(args).await;
    xai_grok_sandbox::flush();
    result.map(|_| ())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_caps_the_core_count() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        assert_eq!(default_worker_threads(nz(360)), DEFAULT_MAX_WORKER_THREADS);
        assert_eq!(default_worker_threads(nz(4)), nz(4));
    }
    #[test]
    fn worker_threads_from_selects_default_or_override() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let cores = nz(360);
        assert_eq!(
            worker_threads_from(None, cores),
            WorkerCount::Accepted(default_worker_threads(cores))
        );
        assert_eq!(
            worker_threads_from(Some("16"), cores),
            WorkerCount::Accepted(nz(16))
        );
    }
    #[test]
    fn override_in_range_is_used_without_a_notice() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let cores = nz(360);
        assert_eq!(
            resolve_worker_override("16", cores),
            WorkerCount::Accepted(nz(16))
        );
        assert_eq!(resolve_worker_override("16", cores).notice(), None);
        assert_eq!(resolve_worker_override(" 8 ", cores).used().get(), 8);
        assert_eq!(
            resolve_worker_override("360", cores),
            WorkerCount::Accepted(cores)
        );
    }
    #[test]
    fn override_out_of_range_is_clamped_with_a_notice() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let cores = nz(360);
        assert_eq!(
            resolve_worker_override("100000", cores),
            WorkerCount::Clamped {
                requested: 100000,
                used: cores,
                cores
            }
        );
        assert_eq!(
            resolve_worker_override("0", cores),
            WorkerCount::Clamped {
                requested: 0,
                used: nz(1),
                cores
            }
        );
        assert_eq!(
            resolve_worker_override("-1", cores),
            WorkerCount::Clamped {
                requested: -1,
                used: nz(1),
                cores
            }
        );
        assert_eq!(
            resolve_worker_override("100000", cores).notice().unwrap(),
            "grok: clamped GROK_WORKER_THREADS=100000 to 360 (valid range is 1..=360)"
        );
    }
    #[test]
    fn override_unparseable_is_ignored_with_a_notice() {
        let cores = NonZeroUsize::new(360).unwrap();
        for value in ["abc", "", "99999999999999999999999999999999999999999"] {
            let ignored = resolve_worker_override(value, cores);
            assert!(matches!(ignored, WorkerCount::Ignored { .. }), "{value}");
            assert_eq!(ignored.used(), default_worker_threads(cores), "{value}");
        }
        assert_eq!(
            resolve_worker_override("abc", cores).notice().unwrap(),
            "grok: ignoring GROK_WORKER_THREADS=\"abc\" (not a valid integer)"
        );
    }
    #[test]
    fn version_output_writer_preserves_channel_aware_contract() {
        xai_grok_version::set_full_version(env!("VERSION_WITH_COMMIT"));
        for (label, expected_suffix) in [
            (" [alpha]", " [alpha]\n"),
            (" [stable]", " [stable]\n"),
            ("", ")\n"),
        ] {
            let mut output = Vec::new();
            write_version(&mut output, label).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.starts_with("grok "));
            assert!(output.contains(env!("VERSION_WITH_COMMIT")));
            assert!(output.ends_with(expected_suffix), "{output:?}");
        }
    }
    #[test]
    fn version_flags_and_doctor_are_distinct_early_intents() {
        let version = PagerArgs::try_parse_from(["grok", "--version"]).unwrap();
        assert!(version.version);
        assert!(version.command.is_none());
        let short = PagerArgs::try_parse_from(["grok", "-v"]).unwrap();
        assert!(short.version);
        assert!(short.command.is_none());
        let subcommand = PagerArgs::try_parse_from(["grok", "version"]).unwrap();
        assert!(!subcommand.version);
        assert!(matches!(
            subcommand.command,
            Some(Command::Version { json: false })
        ));
    }
    #[cfg(all(feature = "jemalloc", unix))]
    struct TempHeapDump(std::path::PathBuf);
    #[cfg(all(feature = "jemalloc", unix))]
    impl TempHeapDump {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "grok-jemalloc-{label}-{}-{}.heap",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
        fn assert_nonempty_dump(&self) {
            let meta = std::fs::metadata(&self.0).expect("dump file missing after prof.dump");
            assert!(meta.len() > 0, "empty dump file");
        }
    }
    #[cfg(all(feature = "jemalloc", unix))]
    impl Drop for TempHeapDump {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    #[cfg(all(feature = "jemalloc", unix))]
    fn require_opt_prof() -> bool {
        if jemalloc_prof_available() {
            return true;
        }
        eprintln!(
            "skip jemalloc prof checks: opt.prof false \
             (release-dist static conf, or MALLOC_CONF=prof:true,prof_active:false,lg_prof_sample={})",
            xai_grok_shell::heap_profile::LG_PROF_SAMPLE
        );
        false
    }
    #[cfg(all(feature = "jemalloc", unix))]
    fn assert_prof_active(expected: bool) {
        assert_eq!(jemalloc_read_prof_active(), Some(expected));
    }
    /// Restores process-global `prof.active` on drop (panic-safe for serial tests).
    #[cfg(all(feature = "jemalloc", unix))]
    struct ProfActiveGuard {
        previous: bool,
    }
    #[cfg(all(feature = "jemalloc", unix))]
    impl ProfActiveGuard {
        fn set(active: bool) -> Self {
            let previous = jemalloc_read_prof_active().unwrap_or(false);
            assert!(
                jemalloc_set_prof_active(active),
                "failed to set prof.active={active}"
            );
            Self { previous }
        }
    }
    #[cfg(all(feature = "jemalloc", unix))]
    impl Drop for ProfActiveGuard {
        fn drop(&mut self) {
            let _ = jemalloc_set_prof_active(self.previous);
        }
    }
    #[cfg(all(feature = "jemalloc", unix))]
    fn assert_stats_sane(stats: xai_grok_shell::heap_profile::JemallocStats) {
        assert!(stats.allocated > 0, "allocated={}", stats.allocated);
        assert!(stats.resident > 0, "resident={}", stats.resident);
        assert!(
            stats.resident >= stats.allocated,
            "resident {} < allocated {}",
            stats.resident,
            stats.allocated
        );
    }
    #[cfg(all(feature = "jemalloc", unix))]
    #[test]
    #[serial_test::serial(jemalloc_heap_profile)]
    fn jemalloc_stats_readable_after_epoch() {
        assert_stats_sane(jemalloc_heap_stats().expect("stats readable"));
    }
    #[cfg(all(feature = "jemalloc", unix))]
    #[test]
    #[serial_test::serial(jemalloc_heap_profile)]
    fn jemalloc_prof_active_round_trip_and_dump() {
        if !require_opt_prof() {
            return;
        }
        assert_prof_active(false);
        {
            let _guard = ProfActiveGuard::set(true);
            assert_prof_active(true);
        }
        assert_prof_active(false);
        let dump = TempHeapDump::new("direct");
        jemalloc_dump_to_path(dump.path()).expect("prof.dump");
        dump.assert_nonempty_dump();
    }
    #[cfg(all(feature = "jemalloc", unix))]
    #[test]
    #[serial_test::serial(jemalloc_heap_profile)]
    fn jemalloc_dump_rejects_interior_nul_path() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        use std::path::Path;
        if !require_opt_prof() {
            return;
        }
        let path = Path::new(OsStr::from_bytes(b"/tmp/grok-jemalloc-\0.heap"));
        let err = jemalloc_dump_to_path(path).expect_err("interior NUL must fail");
        assert!(
            err.to_ascii_lowercase().contains("nul"),
            "unexpected error: {err}"
        );
    }
    #[cfg(all(feature = "jemalloc", unix))]
    #[test]
    #[serial_test::serial(jemalloc_heap_profile)]
    fn install_heap_profile_hooks_wires_shell_apis() {
        install_heap_profile_hooks();
        assert_stats_sane(
            xai_grok_shell::heap_profile::stats().expect("shell stats after install"),
        );
        if !require_opt_prof() {
            assert!(!xai_grok_shell::heap_profile::prof_available());
            return;
        }
        assert!(xai_grok_shell::heap_profile::prof_available());
        assert_prof_active(false);
        {
            let _guard = ProfActiveGuard::set(true);
            assert_prof_active(true);
            assert!(xai_grok_shell::heap_profile::set_prof_active(true));
            assert_prof_active(true);
        }
        assert_prof_active(false);
        assert!(xai_grok_shell::heap_profile::set_prof_active(false));
        assert_prof_active(false);
        let dump = TempHeapDump::new("shell");
        xai_grok_shell::heap_profile::dump_to_path(dump.path()).expect("shell dump");
        dump.assert_nonempty_dump();
    }
    use clap::Parser as _;
    /// `grok dashboard` flags the startup hook.
    /// `grok dashboard` is consumed and launches the interactive TUI with the dashboard open.
    #[serial_test::serial(GROK_AGENT_DASHBOARD)]
    #[test]
    fn dashboard_subcommand_flags_startup() {
        let mut args = PagerArgs::try_parse_from(["grok", "dashboard"]).unwrap();
        flag_dashboard_at_startup_if_requested(&mut args).unwrap();
        assert!(
            args.command.is_none(),
            "soft subcommand must be consumed so the interactive path runs",
        );
        assert_eq!(
            std::env::var("GROK_OPEN_DASHBOARD_AT_STARTUP").as_deref(),
            Ok("1"),
            "startup hook flag must be set",
        );
        unsafe { std::env::remove_var("GROK_OPEN_DASHBOARD_AT_STARTUP") };
    }
    /// `GROK_AGENT_DASHBOARD=0` disables the feature; the subcommand must error visibly before the TUI starts.
    #[serial_test::serial(GROK_AGENT_DASHBOARD)]
    #[test]
    fn dashboard_subcommand_errors_when_disabled() {
        unsafe { std::env::set_var("GROK_AGENT_DASHBOARD", "0") };
        let mut args = PagerArgs::try_parse_from(["grok", "dashboard"]).unwrap();
        let result = flag_dashboard_at_startup_if_requested(&mut args);
        unsafe { std::env::remove_var("GROK_AGENT_DASHBOARD") };
        let err = result.expect_err("disabled dashboard must error");
        assert!(err.to_string().contains("disabled"), "got: {err}");
        assert!(
            std::env::var("GROK_OPEN_DASHBOARD_AT_STARTUP").is_err(),
            "failure path must not flag the startup hook",
        );
    }
    fn multi_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime")
    }
    #[test]
    fn run_and_shutdown_is_fast_without_blocking_work() {
        use std::time::{Duration, Instant};
        let runtime = multi_thread_runtime();
        let grace = Duration::from_secs(5);
        let start = Instant::now();
        let out = run_and_shutdown(runtime, async { 42_u32 }, grace);
        let elapsed = start.elapsed();
        assert_eq!(out, 42, "must pass the future's output through");
        assert!(
            elapsed < grace,
            "clean teardown took {elapsed:?}; grace must be a ceiling, not a floor",
        );
    }
}
