//! The `openab-shepherd` binary: argument parsing and startup over the library.
//!
//! Three entry points share one binary: the bare command (one process owns
//! the panes and serves), `hold` (the pane holder, which starts and restarts
//! the server) and the hidden `front` (the server a holder starts).

use openab_shepherd::audit::{self, Audit};
use openab_shepherd::config::BackendChoice;
use openab_shepherd::http::AppState;
use openab_shepherd::native::{self, NativeBackend, NativeConfig};
use openab_shepherd::profiles::LaunchPolicy;
use openab_shepherd::{config, http, tools};
use std::process::ExitCode;
use std::sync::Arc;

fn usage() -> String {
    format!(
        "\
Usage: openab-shepherd [--host 127.0.0.1] [--port 8765]
       openab-shepherd hold [-- <the same flags>]

`hold` runs the pane holder: it owns the panes and starts the server (with
the flags after `--`) and starts it again when it exits, so panes outlive a
server restart. SIGUSR2 restarts the server only; SIGTERM, SIGINT and SIGHUP
end the panes and the holder. Without `hold`, one process serves and owns its
panes.

Environment:
  OPENAB_SHEPHERD_TOKEN_FILE  path to the bearer token file (preferred)
  OPENAB_SHEPHERD_TOKEN       the bearer token itself (use one of the two)
  OPENAB_SHEPHERD_CLIENTS     clients file (JSON, mode 600, outside any git repo):
                              named clients with scopes; needs the audit file,
                              and refuses the two token variables above
  OPENAB_SHEPHERD_AUDIT_FILE  audit log (JSON Lines) in a mode 700 directory
  OPENAB_SHEPHERD_BACKEND     native, the only backend (default {})
  OPENAB_SHEPHERD_PROFILES    launch profiles file (JSON, outside any git repo);
                              without it start_pane starts nothing
  OPENAB_SHEPHERD_ALLOWED_HOSTS
                              extra Host header values to accept, comma-separated
                              name or name:port (exact, case-insensitive; a name
                              without a port matches any port); default none

Native panes get only HOME, PATH, USER, LANG and TERM from this environment,
plus their launch profile's env, or a caller command's own env.

--host accepts only 127.0.0.1 or ::1. Startup checks run before anything listens.",
        config::DEFAULT_BACKEND
    )
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    host: String,
    port: u16,
    help: bool,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut opts = Options {
        host: config::DEFAULT_HOST.to_string(),
        port: config::DEFAULT_PORT,
        help: false,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        match flag {
            "-h" | "--help" => opts.help = true,
            "--host" => opts.host = value_for(flag, inline, &mut iter)?,
            "--port" => {
                let value = value_for(flag, inline, &mut iter)?;
                opts.port = value
                    .parse::<u16>()
                    .map_err(|_| "--port must be a number from 1 to 65535".to_string())?;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(opts)
}

fn value_for(
    flag: &str,
    inline: Option<String>,
    iter: &mut std::slice::Iter<'_, String>,
) -> Result<String, String> {
    match inline {
        Some(value) => Ok(value),
        None => iter
            .next()
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value")),
    }
}

/// What startup reads before anything listens: the settings, the launch
/// policy and the audit log. The error is the exit status to return, after
/// its message is printed.
struct Startup {
    config: config::Config,
    policy: LaunchPolicy,
    audit: Option<Arc<Audit>>,
}

fn load_startup(opts: &Options) -> Result<Startup, ExitCode> {
    let project_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("openab-shepherd: refusing to start: cannot read current directory: {err}");
            return Err(ExitCode::from(2));
        }
    };

    let config = match config::load_config(
        |key| std::env::var(key).ok(),
        &opts.host,
        opts.port,
        &project_dir,
    ) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("openab-shepherd: refusing to start: {err}");
            return Err(ExitCode::from(2));
        }
    };

    let policy = match LaunchPolicy::load(|key| std::env::var(key).ok(), &project_dir) {
        Ok(policy) => policy,
        Err(err) => {
            eprintln!("openab-shepherd: refusing to start: {err}");
            return Err(ExitCode::from(2));
        }
    };

    let audit = match &config.audit_file {
        Some(path) => match Audit::open_file(path, &project_dir, audit::system_clock()) {
            Ok(audit) => Some(Arc::new(audit)),
            Err(err) => {
                eprintln!("openab-shepherd: refusing to start: {err}");
                return Err(ExitCode::from(2));
            }
        },
        None => None,
    };
    Ok(Startup {
        config,
        policy,
        audit,
    })
}

/// `openab-shepherd hold [-- <server flags>]`: the pane holder.
fn hold_main(args: &[String]) -> ExitCode {
    let front_args = match args.split_first() {
        None => Vec::new(),
        Some((first, rest)) if first == "--" => rest.to_vec(),
        Some((first, _)) if first == "-h" || first == "--help" => {
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        Some((first, _)) => {
            eprintln!(
                "openab-shepherd: hold: unexpected argument {first:?} \
                 (the server's flags go after --)"
            );
            eprintln!("{}", usage());
            return ExitCode::from(2);
        }
    };
    ExitCode::from(native::run_holder(front_args) as u8)
}

/// The hidden flags of `front`, and the server's own.
#[derive(Debug, PartialEq, Eq)]
struct FrontOptions {
    ops_fd: i32,
    events_fd: i32,
    /// Attach and adopt, but serve nothing (tests and operator scripts).
    no_listen: bool,
    /// With `no_listen`: start `NAME=COMMAND` as a pane unless the holder
    /// already has a pane called NAME.
    test_panes: Vec<(String, String)>,
    server: Options,
}

fn parse_front_args(args: &[String]) -> Result<FrontOptions, String> {
    let (mut ops_fd, mut events_fd) = (None, None);
    let mut no_listen = false;
    let mut test_panes = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut iter = args.iter();
    let fd = |value: String, flag: &str| {
        value
            .parse::<i32>()
            .ok()
            .filter(|n| *n >= 3)
            .ok_or_else(|| format!("{flag} must be a descriptor number of 3 or more"))
    };
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        match flag {
            "--ops-fd" => ops_fd = Some(fd(value_for(flag, inline, &mut iter)?, flag)?),
            "--events-fd" => events_fd = Some(fd(value_for(flag, inline, &mut iter)?, flag)?),
            "--no-listen" => no_listen = true,
            "--test-pane" => {
                let value = value_for(flag, inline, &mut iter)?;
                let (name, command) = value
                    .split_once('=')
                    .ok_or_else(|| "--test-pane takes NAME=COMMAND".to_string())?;
                test_panes.push((name.to_string(), command.to_string()));
            }
            _ => rest.push(arg.clone()),
        }
    }
    let (Some(ops_fd), Some(events_fd)) = (ops_fd, events_fd) else {
        return Err("front needs --ops-fd and --events-fd (the holder starts it)".to_string());
    };
    if ops_fd == events_fd {
        return Err("--ops-fd and --events-fd must differ".to_string());
    }
    Ok(FrontOptions {
        ops_fd,
        events_fd,
        no_listen,
        test_panes,
        server: parse_args(&rest)?,
    })
}

/// `openab-shepherd front --ops-fd N --events-fd M [server flags]`: the
/// server a holder starts. It never ends panes: a signal, a panic, the end of
/// the server or the link closing exits it, and the holder starts the next.
fn front_main(args: &[String]) -> ExitCode {
    let opts = match parse_front_args(args) {
        Ok(opts) => opts,
        Err(message) => {
            eprintln!("openab-shepherd: front: {message}");
            return ExitCode::from(2);
        }
    };
    // Before any other thread starts, so they all leave these signals to the
    // handler, which exits without ending the panes.
    if let Err(err) = native::exit_on_signals_leaving_panes() {
        eprintln!("openab-shepherd: front: cannot install the signal handler ({err})");
        return ExitCode::from(2);
    }
    let version =
        match native::front_version(std::env::var(native::TEST_VERSION_ENV).ok().as_deref()) {
            Ok(version) => version,
            Err(message) => {
                eprintln!("openab-shepherd: front: {message}");
                return ExitCode::from(2);
            }
        };
    // Settings are read on every front start, so a changed file takes effect
    // on the next restart.
    let startup = if opts.no_listen {
        None
    } else {
        match load_startup(&opts.server) {
            Ok(startup) => Some(startup),
            Err(code) => return code,
        }
    };
    // SAFETY: the holder passes these two descriptors to this process and
    // nothing else here uses them.
    let link = match unsafe { native::connect_front(opts.ops_fd, opts.events_fd, version) } {
        Ok(link) => link,
        Err(err @ native::FrontError::VersionMismatch { .. }) => {
            eprintln!("openab-shepherd: front: {err}");
            return ExitCode::from(native::EXIT_VERSION_MISMATCH as u8);
        }
        Err(err) => {
            eprintln!("openab-shepherd: front: {err}");
            return ExitCode::from(native::EXIT_LINK_CLOSED as u8);
        }
    };
    // Either socket closing means the holder is gone: exit at once, panes
    // untouched.
    {
        let watch = link.watch.clone();
        let started = std::thread::Builder::new()
            .name("front-link-watch".to_string())
            .spawn(move || {
                watch.wait_closed();
                eprintln!("openab-shepherd: front: the holder link closed; exiting");
                std::process::exit(native::EXIT_LINK_CLOSED);
            });
        if let Err(err) = started {
            eprintln!("openab-shepherd: front: cannot watch the holder link ({err})");
            return ExitCode::from(1);
        }
    }
    eprintln!(
        "openab-shepherd: front: attached; adopted {} pane(s), {} replayed",
        link.adopted.len(),
        link.replayed.len()
    );

    let Some(startup) = startup else {
        return idle_front(&link, &opts.test_panes);
    };
    let Startup {
        config,
        policy,
        audit,
    } = startup;
    let native = link.backend.clone();
    let mut shepherd =
        tools::Shepherd::with_policy(native.clone(), policy).with_registry(Arc::new(native));
    // Before the routine starts, so its nudges are recorded too.
    if let Some(audit) = audit {
        shepherd = shepherd.with_audit(audit);
    }
    // The tools layer rebuilds from the panes' metadata, then the routine
    // starts watching the panes it finds.
    shepherd.adopt_registry();
    shepherd.spawn_routine();
    let state = Arc::new(AppState {
        clients: config.clients.clone(),
        allowed_hosts: config.allowed_hosts.clone(),
        shepherd,
    });
    eprintln!(
        "openab-shepherd: listening on http://{}{} (backend: {}, panes held by the holder)",
        config.bind,
        http::MCP_PATH,
        config.backend.name()
    );
    if !config.allowed_hosts.is_empty() {
        let list: Vec<String> = config.allowed_hosts.iter().map(|h| h.to_string()).collect();
        eprintln!(
            "openab-shepherd: extra allowed Host values ({}): {}",
            config::ALLOWED_HOSTS_ENV,
            list.join(", ")
        );
    }
    if let Err(err) = http::serve(config.bind, state) {
        // The panes are the holder's: leave them.
        eprintln!("openab-shepherd: {err}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// The `--no-listen` front: report what it adopted, start the test panes,
/// then wait for the link to close.
fn idle_front(link: &native::FrontLink, test_panes: &[(String, String)]) -> ExitCode {
    use openab_shepherd::backend::{RunSpec, TerminalBackend};
    let named = |name: &str| {
        link.backend
            .list(true)
            .map(|list| {
                list.terminals
                    .iter()
                    .any(|t| t.name.as_deref() == Some(name))
            })
            .unwrap_or(false)
    };
    for (name, command) in test_panes {
        if named(name) {
            continue;
        }
        let spec = RunSpec {
            line: Some(command.clone()),
            name: Some(name.clone()),
            ..RunSpec::default()
        };
        match link.backend.run(&spec) {
            Ok(_) => eprintln!("openab-shepherd: front: started pane {name}"),
            Err(err) => {
                eprintln!(
                    "openab-shepherd: front: cannot start pane {name}: {}",
                    err.detail
                );
                return ExitCode::from(1);
            }
        }
    }
    if let Ok(list) = link.backend.list(true) {
        for t in &list.terminals {
            eprintln!(
                "openab-shepherd: front: pane {} live={} exit_code={:?}",
                t.name.as_deref().unwrap_or("?"),
                t.live,
                t.exit_code
            );
        }
    }
    link.watch.wait_closed();
    ExitCode::from(native::EXIT_LINK_CLOSED as u8)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("hold") => return hold_main(&args[1..]),
        Some("front") => return front_main(&args[1..]),
        _ => {}
    }
    let opts = match parse_args(&args) {
        Ok(opts) => opts,
        Err(message) => {
            eprintln!("openab-shepherd: {message}");
            eprintln!("{}", usage());
            return ExitCode::from(2);
        }
    };
    if opts.help {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    }

    let Startup {
        config,
        policy,
        audit,
    } = match load_startup(&opts) {
        Ok(startup) => startup,
        Err(code) => return code,
    };

    let backend_name = config.backend.name();
    let (shepherd, native_handle) = match config.backend {
        BackendChoice::Native => {
            let native = NativeBackend::new(NativeConfig::default());
            // Before any other thread starts, so all of them leave these
            // signals to the handler, which ends the panes before exiting.
            if let Err(err) = native.end_panes_on_exit_signals() {
                eprintln!(
                    "openab-shepherd: refusing to start: cannot install the signal handler \
                     that ends native panes on exit ({err})"
                );
                return ExitCode::from(2);
            }
            // The signal thread keeps a handle, so the panes are never
            // dropped: end them on a main-thread panic and a serve error.
            let on_panic = native.clone();
            let default_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                default_hook(info);
                if std::thread::current().name() == Some("main") {
                    on_panic.shutdown();
                }
            }));
            let handle = Some(native.clone());
            let mut shepherd = tools::Shepherd::with_policy(native, policy);
            // Before the routine starts, so its nudges are recorded too.
            if let Some(audit) = audit {
                shepherd = shepherd.with_audit(audit);
            }
            // No thread, and nothing to watch, when no profile sets a routine.
            shepherd.spawn_routine();
            (shepherd, handle)
        }
    };
    let state = Arc::new(AppState {
        clients: config.clients.clone(),
        allowed_hosts: config.allowed_hosts.clone(),
        shepherd,
    });
    eprintln!(
        "openab-shepherd: listening on http://{}{} (backend: {backend_name})",
        config.bind,
        http::MCP_PATH
    );
    if !config.allowed_hosts.is_empty() {
        let list: Vec<String> = config.allowed_hosts.iter().map(|h| h.to_string()).collect();
        eprintln!(
            "openab-shepherd: extra allowed Host values ({}): {}",
            config::ALLOWED_HOSTS_ENV,
            list.join(", ")
        );
    }
    if let Err(err) = http::serve(config.bind, state) {
        eprintln!("openab-shepherd: {err}");
        if let Some(native) = &native_handle {
            native.shutdown();
        }
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_are_loopback_and_8765() {
        let opts = parse_args(&[]).unwrap();
        assert_eq!(opts.host, "127.0.0.1");
        assert_eq!(opts.port, 8765);
    }

    #[test]
    fn host_and_port_accept_both_forms() {
        let opts = parse_args(&args(&["--host", "::1", "--port=9000"])).unwrap();
        assert_eq!(opts.host, "::1");
        assert_eq!(opts.port, 9000);
    }

    #[test]
    fn bad_port_and_unknown_flags_are_errors() {
        assert!(parse_args(&args(&["--port", "70000"])).is_err());
        assert!(parse_args(&args(&["--port", "abc"])).is_err());
        assert!(parse_args(&args(&["--bogus"])).is_err());
        assert!(parse_args(&args(&["--host"])).is_err());
    }

    #[test]
    fn the_hidden_front_flags_do_not_exist_on_the_bare_command() {
        for flag in ["--ops-fd", "--events-fd", "--no-listen", "--test-pane"] {
            assert!(parse_args(&args(&[flag, "5"])).is_err(), "{flag}");
        }
    }

    #[test]
    fn a_front_needs_both_descriptors_and_takes_the_server_flags() {
        assert!(parse_front_args(&args(&["--ops-fd", "3"])).is_err());
        assert!(parse_front_args(&args(&["--ops-fd", "3", "--events-fd", "3"])).is_err());
        assert!(parse_front_args(&args(&["--ops-fd", "1", "--events-fd", "4"])).is_err());
        let opts =
            parse_front_args(&args(&["--ops-fd", "3", "--events-fd=4", "--port", "9000"])).unwrap();
        assert_eq!((opts.ops_fd, opts.events_fd), (3, 4));
        assert_eq!(opts.server.port, 9000);
        assert!(!opts.no_listen);
        // An unknown flag is still an error, as on the bare command.
        assert!(
            parse_front_args(&args(&["--ops-fd", "3", "--events-fd", "4", "--bogus"])).is_err()
        );
    }

    #[test]
    fn a_test_pane_is_name_equals_command() {
        let opts = parse_front_args(&args(&[
            "--ops-fd",
            "3",
            "--events-fd",
            "4",
            "--no-listen",
            "--test-pane",
            "work=sleep 5; echo a=b",
        ]))
        .unwrap();
        assert!(opts.no_listen);
        assert_eq!(
            opts.test_panes,
            vec![("work".to_string(), "sleep 5; echo a=b".to_string())]
        );
        assert!(parse_front_args(&args(&[
            "--ops-fd",
            "3",
            "--events-fd",
            "4",
            "--test-pane",
            "nocommand"
        ]))
        .is_err());
    }
}
