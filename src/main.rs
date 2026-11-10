//! OpenAB Shepherd: an MCP server that exposes herdr-core terminal panes as tools.
//!
//! The server listens on loopback only, requires a bearer token, and forwards each
//! tool call to herdr-core over its Unix socket. See README.md for the tool map.

mod ansi;
mod config;
mod herdr;
mod http;
mod input;
mod mcp;
mod tools;

#[cfg(test)]
mod testutil;

use herdr::HerdrClient;
use http::AppState;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
Usage: openab-shepherd [--host 127.0.0.1] [--port 8765]

Environment:
  OPENAB_SHEPHERD_TOKEN_FILE  path to the bearer token file (preferred)
  OPENAB_SHEPHERD_TOKEN       the bearer token itself (use one of the two)
  HC_SOCKET                   herdr-core Unix socket path (required)

--host accepts only 127.0.0.1 or ::1. Startup checks run before anything listens.";

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
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    if opts.help {
        println!("{USAGE}");
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

    let state = Arc::new(AppState {
        token: config.token,
        client: HerdrClient::new(config.socket_path),
    });
    eprintln!(
        "openab-shepherd: listening on http://{}{}",
        config.bind,
        http::MCP_PATH
    );
    if let Err(err) = http::serve(config.bind, state) {
        eprintln!("openab-shepherd: {err}");
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
