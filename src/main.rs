//! The `openab-shepherd` binary: argument parsing and startup over the library.

use openab_shepherd::config::BackendChoice;
use openab_shepherd::hc::HcBackend;
use openab_shepherd::http::AppState;
use openab_shepherd::native::{NativeBackend, NativeConfig};
use openab_shepherd::profiles::LaunchPolicy;
use openab_shepherd::{config, http, tools};
use std::process::ExitCode;
use std::sync::Arc;

fn usage() -> String {
    format!(
        "\
Usage: openab-shepherd [--host 127.0.0.1] [--port 8765]

Environment:
  OPENAB_SHEPHERD_TOKEN_FILE  path to the bearer token file (preferred)
  OPENAB_SHEPHERD_TOKEN       the bearer token itself (use one of the two)
  OPENAB_SHEPHERD_BACKEND     native or hc (default {})
  OPENAB_SHEPHERD_PROFILES    launch profiles file (JSON, outside any git repo);
                              without it start_pane starts nothing
  HC_SOCKET                   herdr-core Unix socket path (required for hc only)

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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
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

    let project_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("openab-shepherd: refusing to start: cannot read current directory: {err}");
            return ExitCode::from(2);
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
            return ExitCode::from(2);
        }
    };

    let policy = match LaunchPolicy::load(|key| std::env::var(key).ok(), &project_dir) {
        Ok(policy) => policy,
        Err(err) => {
            eprintln!("openab-shepherd: refusing to start: {err}");
            return ExitCode::from(2);
        }
    };

    let backend_name = config.backend.name();
    let mut native_handle = None;
    let shepherd = match config.backend {
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
            native_handle = Some(native.clone());
            tools::Shepherd::with_policy(native, policy)
        }
        BackendChoice::Hc { socket } => {
            tools::Shepherd::with_policy(HcBackend::new(socket), policy)
        }
    };
    let state = Arc::new(AppState {
        token: config.token,
        shepherd,
    });
    eprintln!(
        "openab-shepherd: listening on http://{}{} (backend: {backend_name})",
        config.bind,
        http::MCP_PATH
    );
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
}
