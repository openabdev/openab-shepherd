//! Startup checks: bearer token, bind address, and the terminal backend (with
//! the herdr-core socket path when the backend is hc).
//!
//! Every check runs before the server opens a listening socket. A failed check
//! makes the binary exit with status 2 and print the reason on stderr.

use std::fmt;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const TOKEN_FILE_ENV: &str = "OPENAB_SHEPHERD_TOKEN_FILE";
pub const TOKEN_ENV: &str = "OPENAB_SHEPHERD_TOKEN";
/// herdr-core socket override. The name appears in the herdr-core binary's own
/// error text. Its precedence against HC_SESSION and the config file is unverified,
/// so this server reads only this variable.
pub const SOCKET_ENV: &str = "HC_SOCKET";
/// Which terminal backend runs the panes: `native` or `hc`.
pub const BACKEND_ENV: &str = "OPENAB_SHEPHERD_BACKEND";
/// The backend used when [`BACKEND_ENV`] is unset or empty. `native` since the
/// real-Kiro probe passed (kiro-cli 3.1.0, examples/native_probe.rs).
pub const DEFAULT_BACKEND: &str = "native";
/// Extra Host header values to accept besides the loopback names, as a
/// comma-separated list of `name` or `name:port`. Unset or empty means none.
pub const ALLOWED_HOSTS_ENV: &str = "OPENAB_SHEPHERD_ALLOWED_HOSTS";
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8765;
pub const MIN_TOKEN_LENGTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// The terminal backend chosen at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendChoice {
    /// herdr-core over its Unix socket.
    Hc { socket: PathBuf },
    /// Shepherd's own PTY core; needs no socket.
    Native,
}

impl BackendChoice {
    /// The value of [`BACKEND_ENV`] that selects it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Hc { .. } => "hc",
            Self::Native => "native",
        }
    }
}

/// One extra Host header value the operator allows, for example the name a
/// `tailscale serve` front end forwards. `name` is lowercase with no trailing
/// dot. A `None` port matches the name on any port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedHost {
    pub name: String,
    pub port: Option<u16>,
}

impl fmt::Display for AllowedHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.port {
            Some(port) => write!(f, "{}:{port}", self.name),
            None => write!(f, "{} (any port)", self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub token: String,
    pub backend: BackendChoice,
    pub bind: SocketAddr,
    pub allowed_hosts: Vec<AllowedHost>,
}

/// Validate every startup input. `lookup` reads an environment variable, and
/// `project_dir` is the folder the server runs from (its git repository is also
/// a boundary for the token file).
pub fn load_config<F>(
    lookup: F,
    host: &str,
    port: u16,
    project_dir: &Path,
) -> Result<Config, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let ip = check_bind_host(host)?;
    if port == 0 {
        return Err(ConfigError("port 0 is not allowed".to_string()));
    }
    let token = load_token(&lookup, project_dir)?;
    let backend = load_backend(&lookup)?;
    let allowed_hosts = parse_allowed_hosts(lookup(ALLOWED_HOSTS_ENV).as_deref().unwrap_or(""))?;
    Ok(Config {
        token,
        backend,
        bind: SocketAddr::new(ip, port),
        allowed_hosts,
    })
}

/// Parse [`ALLOWED_HOSTS_ENV`]. An empty or all-whitespace value means no extra
/// hosts. Each comma-separated item is trimmed and must be `name` or
/// `name:port`: letters, digits, `-` and `.` only, no empty labels, port 1 to
/// 65535. A single trailing dot on the name is dropped and the name is
/// lowercased. Anything else (an empty item, a scheme, a path, a wildcard,
/// whitespace inside an item, brackets) refuses the whole value.
pub fn parse_allowed_hosts(value: &str) -> Result<Vec<AllowedHost>, ConfigError> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    value.split(',').map(parse_allowed_host).collect()
}

fn parse_allowed_host(item: &str) -> Result<AllowedHost, ConfigError> {
    let entry = item.trim();
    let invalid = |why: &str| {
        ConfigError(format!(
            "{ALLOWED_HOSTS_ENV}: invalid entry {entry:?}: {why}"
        ))
    };
    if entry.is_empty() {
        return Err(invalid("empty item"));
    }
    if entry.contains("://") {
        return Err(invalid("no scheme allowed; give a host name"));
    }
    if entry.contains('/') {
        return Err(invalid("no path allowed; give a host name"));
    }
    if entry.contains('*') {
        return Err(invalid("wildcards are not allowed"));
    }
    if entry.chars().any(char::is_whitespace) {
        return Err(invalid("whitespace inside the entry"));
    }
    let (name, port) = match entry.split_once(':') {
        None => (entry, None),
        Some((name, port)) => {
            let port = port
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0 && port.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| invalid("port must be a number from 1 to 65535"))?;
            (name, Some(port))
        }
    };
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() {
        return Err(invalid("empty host name"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return Err(invalid(
            "host name may contain only letters, digits, '-' and '.'",
        ));
    }
    if name.split('.').any(str::is_empty) {
        return Err(invalid("empty label in host name"));
    }
    Ok(AllowedHost {
        name: name.to_ascii_lowercase(),
        port,
    })
}

/// Accept only the literal loopback addresses. Names such as `localhost` are refused.
pub fn check_bind_host(host: &str) -> Result<IpAddr, ConfigError> {
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        if v4 == Ipv4Addr::LOCALHOST {
            return Ok(IpAddr::V4(v4));
        }
    } else if let Ok(v6) = host.parse::<Ipv6Addr>() {
        if v6 == Ipv6Addr::LOCALHOST {
            return Ok(IpAddr::V6(v6));
        }
    }
    Err(ConfigError(format!(
        "refusing to bind {host:?}: only 127.0.0.1 and ::1 are allowed"
    )))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

fn load_token<F>(lookup: &F, project_dir: &Path) -> Result<String, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let file = non_empty(lookup(TOKEN_FILE_ENV));
    let direct = non_empty(lookup(TOKEN_ENV));
    if file.is_some() && direct.is_some() {
        return Err(ConfigError(format!(
            "set only one of {TOKEN_FILE_ENV} or {TOKEN_ENV}, not both"
        )));
    }
    let token = match (file, direct) {
        (Some(path), None) => read_token_file(Path::new(&path), project_dir)?,
        (None, Some(value)) => value.trim().to_string(),
        _ => {
            return Err(ConfigError(format!(
                "no bearer token: set {TOKEN_FILE_ENV} (preferred) or {TOKEN_ENV}"
            )))
        }
    };
    if token.len() < MIN_TOKEN_LENGTH {
        return Err(ConfigError(format!(
            "token must be at least {MIN_TOKEN_LENGTH} characters"
        )));
    }
    Ok(token)
}

fn read_token_file(path: &Path, project_dir: &Path) -> Result<String, ConfigError> {
    // Resolve symlinks first, so a link inside the repo cannot hide a file outside it.
    let canonical = fs::canonicalize(path)
        .map_err(|err| ConfigError(format!("cannot read token file: {err}")))?;
    check_outside_repo(&canonical, project_dir, "token file")?;
    let meta = fs::metadata(&canonical)
        .map_err(|err| ConfigError(format!("cannot read token file: {err}")))?;
    if !meta.is_file() {
        return Err(ConfigError("token file must be a regular file".to_string()));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(ConfigError(
            "token file must not be readable by group or others (chmod 600)".to_string(),
        ));
    }
    let text = fs::read_to_string(&canonical)
        .map_err(|err| ConfigError(format!("cannot read token file: {err}")))?;
    Ok(text.trim().to_string())
}

/// Refuse a file (the token file, the launch profiles) that is inside the
/// project folder, or inside any git repository that the project folder or
/// the file's own folder belongs to. `token` is already canonical.
/// Repository roots come from `git rev-parse --show-toplevel`, not from a path prefix.
pub(crate) fn check_outside_repo(
    token: &Path,
    project_dir: &Path,
    what: &str,
) -> Result<(), ConfigError> {
    let project = fs::canonicalize(project_dir)
        .map_err(|err| ConfigError(format!("cannot resolve project folder: {err}")))?;
    let mut boundaries = vec![project.clone()];
    if let Some(root) = git_toplevel(&project)? {
        boundaries.push(root);
    }
    if let Some(dir) = token.parent() {
        if let Some(root) = git_toplevel(dir)? {
            boundaries.push(root);
        }
    }
    if boundaries
        .iter()
        .any(|boundary| token.starts_with(boundary))
    {
        return Err(ConfigError(format!(
            "{what} must be outside the git repository and the project folder"
        )));
    }
    Ok(())
}

/// Returns the work-tree root of the repository containing `dir`, or `None` when
/// `dir` is not inside a repository. Any other git failure is an error, so the
/// server refuses to start rather than guessing.
fn git_toplevel(dir: &Path) -> Result<Option<PathBuf>, ConfigError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|err| ConfigError(format!("cannot run git to check the token location: {err}")))?;
    if output.status.success() {
        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok(if root.is_empty() {
            None
        } else {
            Some(PathBuf::from(root))
        });
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not a git repository") {
        Ok(None)
    } else {
        Err(ConfigError(format!(
            "git rev-parse failed while checking the token location: {}",
            stderr.trim()
        )))
    }
}

/// `native` needs nothing else (HC_SOCKET is ignored); `hc` needs HC_SOCKET.
fn load_backend<F>(lookup: &F) -> Result<BackendChoice, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    let value = non_empty(lookup(BACKEND_ENV).map(|v| v.trim().to_string()));
    match value.as_deref().unwrap_or(DEFAULT_BACKEND) {
        "native" => Ok(BackendChoice::Native),
        "hc" => Ok(BackendChoice::Hc {
            socket: load_socket_path(lookup)?,
        }),
        _ => Err(ConfigError(format!("{BACKEND_ENV} must be native or hc"))),
    }
}

fn load_socket_path<F>(lookup: &F) -> Result<PathBuf, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    match non_empty(lookup(SOCKET_ENV).map(|v| v.trim().to_string())) {
        Some(path) => Ok(PathBuf::from(path)),
        None => Err(ConfigError(format!(
            "no herdr-core socket: set {SOCKET_ENV} to the socket path"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{git_init, TempDir};
    use std::collections::HashMap;
    use std::fs::Permissions;

    // Example values only; not credentials.
    const GOOD_TOKEN: &str = "test-token-0123456789abcdef";
    const SOCKET: &str = "/tmp/shepherd-test-absent.sock";

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn write_token(path: &Path, mode: u32) {
        fs::write(path, format!("{GOOD_TOKEN}\n")).unwrap();
        fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
    }

    fn load_with_token_file(token: &Path, project: &Path) -> Result<Config, ConfigError> {
        let token = token.to_string_lossy().to_string();
        load_config(
            env_from(&[(TOKEN_FILE_ENV, token.as_str()), (SOCKET_ENV, SOCKET)]),
            "127.0.0.1",
            DEFAULT_PORT,
            project,
        )
    }

    #[test]
    fn refuses_without_any_token() {
        let dir = TempDir::new("no-token");
        let err = load_config(
            env_from(&[(SOCKET_ENV, SOCKET)]),
            "127.0.0.1",
            8765,
            dir.path(),
        )
        .unwrap_err();
        assert!(err.0.contains("no bearer token"), "{err}");
    }

    #[test]
    fn refuses_short_token() {
        let dir = TempDir::new("short");
        let err = load_config(
            env_from(&[(TOKEN_ENV, "short"), (SOCKET_ENV, SOCKET)]),
            "127.0.0.1",
            8765,
            dir.path(),
        )
        .unwrap_err();
        assert!(err.0.contains("at least"), "{err}");
    }

    #[test]
    fn token_env_and_socket_accepted() {
        let dir = TempDir::new("env-ok");
        let config = load_config(
            env_from(&[
                (TOKEN_ENV, GOOD_TOKEN),
                (SOCKET_ENV, SOCKET),
                (BACKEND_ENV, "hc"),
            ]),
            "127.0.0.1",
            8765,
            dir.path(),
        )
        .unwrap();
        assert_eq!(config.token, GOOD_TOKEN);
        assert_eq!(
            config.backend,
            BackendChoice::Hc {
                socket: PathBuf::from(SOCKET)
            }
        );
        assert_eq!(config.bind, "127.0.0.1:8765".parse().unwrap());
    }

    #[test]
    fn native_backend_needs_no_socket() {
        let dir = TempDir::new("native");
        for env in [
            vec![(TOKEN_ENV, GOOD_TOKEN), (BACKEND_ENV, "native")],
            vec![
                (TOKEN_ENV, GOOD_TOKEN),
                (BACKEND_ENV, " native "),
                (SOCKET_ENV, SOCKET),
            ],
        ] {
            let config = load_config(env_from(&env), "127.0.0.1", 8765, dir.path()).unwrap();
            assert_eq!(config.backend, BackendChoice::Native);
            assert_eq!(config.backend.name(), "native");
        }
    }

    #[test]
    fn unknown_backend_refused() {
        let dir = TempDir::new("bad-backend");
        for value in ["tmux", "NATIVE", "native,hc"] {
            let err = load_config(
                env_from(&[
                    (TOKEN_ENV, GOOD_TOKEN),
                    (SOCKET_ENV, SOCKET),
                    (BACKEND_ENV, value),
                ]),
                "127.0.0.1",
                8765,
                dir.path(),
            )
            .unwrap_err();
            assert_eq!(
                err.0, "OPENAB_SHEPHERD_BACKEND must be native or hc",
                "{value}"
            );
        }
    }

    #[test]
    fn unset_backend_uses_the_default() {
        let dir = TempDir::new("default-backend");
        for env in [
            vec![(TOKEN_ENV, GOOD_TOKEN), (SOCKET_ENV, SOCKET)],
            vec![
                (TOKEN_ENV, GOOD_TOKEN),
                (SOCKET_ENV, SOCKET),
                (BACKEND_ENV, "  "),
            ],
        ] {
            let config = load_config(env_from(&env), "127.0.0.1", 8765, dir.path()).unwrap();
            assert_eq!(config.backend.name(), DEFAULT_BACKEND);
        }
    }

    #[test]
    fn both_token_sources_refused() {
        let dir = TempDir::new("both");
        let token = dir.path().join("t");
        write_token(&token, 0o600);
        let token_text = token.to_string_lossy().to_string();
        let err = load_config(
            env_from(&[
                (TOKEN_FILE_ENV, token_text.as_str()),
                (TOKEN_ENV, GOOD_TOKEN),
                (SOCKET_ENV, SOCKET),
            ]),
            "127.0.0.1",
            8765,
            dir.path(),
        )
        .unwrap_err();
        assert!(err.0.contains("not both"), "{err}");
    }

    #[test]
    fn missing_socket_env_refused() {
        let dir = TempDir::new("no-socket");
        let err = load_config(
            env_from(&[(TOKEN_ENV, GOOD_TOKEN), (BACKEND_ENV, "hc")]),
            "127.0.0.1",
            8765,
            dir.path(),
        )
        .unwrap_err();
        assert!(err.0.contains("HC_SOCKET"), "{err}");
    }

    #[test]
    fn token_file_readable_by_others_refused() {
        let project = TempDir::new("loose-project");
        let outside = TempDir::new("loose");
        let token = outside.path().join("loose.token");
        write_token(&token, 0o644);
        let err = load_with_token_file(&token, project.path()).unwrap_err();
        assert!(err.0.contains("chmod 600"), "{err}");
    }

    #[test]
    fn token_file_strict_mode_accepted() {
        let project = TempDir::new("strict-project");
        let outside = TempDir::new("strict");
        let token = outside.path().join("ok.token");
        write_token(&token, 0o600);
        let config = load_with_token_file(&token, project.path()).unwrap();
        assert_eq!(config.token, GOOD_TOKEN);
    }

    #[test]
    fn token_file_inside_project_folder_refused() {
        let dir = TempDir::new("in-project");
        let token = dir.path().join("inside.token");
        write_token(&token, 0o600);
        let err = load_with_token_file(&token, dir.path()).unwrap_err();
        assert!(err.0.contains("outside the git repository"), "{err}");
    }

    #[test]
    fn token_file_inside_enclosing_git_repo_refused() {
        // Repo at <tmp>/repo, project at <tmp>/repo/openab-shepherd, token at the
        // repo root: outside the project folder, but inside the repository.
        let base = TempDir::new("repo-refuse");
        let repo = base.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git_init(&repo);
        let project = repo.join("openab-shepherd");
        fs::create_dir_all(&project).unwrap();
        let token = repo.join("shepherd-secret.token");
        write_token(&token, 0o600);

        let err = load_with_token_file(&token, &project).unwrap_err();
        assert!(err.0.contains("outside the git repository"), "{err}");
    }

    #[test]
    fn token_file_outside_enclosing_git_repo_accepted() {
        let base = TempDir::new("repo-accept");
        let repo = base.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git_init(&repo);
        let project = repo.join("openab-shepherd");
        fs::create_dir_all(&project).unwrap();

        let outside = TempDir::new("outside-token");
        let token = outside.path().join("outside.token");
        write_token(&token, 0o600);
        assert_eq!(
            load_with_token_file(&token, &project).unwrap().token,
            GOOD_TOKEN
        );
    }

    #[test]
    fn loopback_hosts_allowed() {
        assert_eq!(
            check_bind_host("127.0.0.1").unwrap(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            check_bind_host("::1").unwrap(),
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        );
    }

    #[test]
    fn non_loopback_hosts_refused_before_token_check() {
        // RFC 5737 documentation addresses and a name; none of them may bind.
        for host in [
            "0.0.0.0",
            "::",
            "192.0.2.10",
            "198.51.100.7",
            "203.0.113.5",
            "localhost",
            "127.0.0.2",
        ] {
            let err = check_bind_host(host).unwrap_err();
            assert!(err.0.contains("only 127.0.0.1 and ::1"), "{host}: {err}");
        }
        // Full config path: a bad host fails even with a valid token and socket.
        let dir = TempDir::new("bind-refuse");
        let err = load_config(
            env_from(&[(TOKEN_ENV, GOOD_TOKEN), (SOCKET_ENV, SOCKET)]),
            "0.0.0.0",
            8765,
            dir.path(),
        )
        .unwrap_err();
        assert!(err.0.contains("refusing to bind"), "{err}");
    }

    #[test]
    fn port_zero_refused() {
        let dir = TempDir::new("port0");
        let err = load_config(
            env_from(&[(TOKEN_ENV, GOOD_TOKEN), (SOCKET_ENV, SOCKET)]),
            "127.0.0.1",
            0,
            dir.path(),
        )
        .unwrap_err();
        assert!(err.0.contains("port 0"), "{err}");
    }

    fn host(name: &str, port: Option<u16>) -> AllowedHost {
        AllowedHost {
            name: name.to_string(),
            port,
        }
    }

    #[test]
    fn allowed_hosts_default_to_none() {
        let dir = TempDir::new("allowed-default");
        for env in [
            vec![(TOKEN_ENV, GOOD_TOKEN)],
            vec![(TOKEN_ENV, GOOD_TOKEN), (ALLOWED_HOSTS_ENV, "")],
            vec![(TOKEN_ENV, GOOD_TOKEN), (ALLOWED_HOSTS_ENV, "  ")],
        ] {
            let config = load_config(env_from(&env), "127.0.0.1", 8765, dir.path()).unwrap();
            assert!(config.allowed_hosts.is_empty());
        }
    }

    #[test]
    fn allowed_hosts_parsed_and_normalised() {
        let dir = TempDir::new("allowed-ok");
        let config = load_config(
            env_from(&[
                (TOKEN_ENV, GOOD_TOKEN),
                (
                    ALLOWED_HOSTS_ENV,
                    " MCP-Host.Example-Tailnet.ts.net.:8445 , other.example ",
                ),
            ]),
            "127.0.0.1",
            8765,
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            config.allowed_hosts,
            vec![
                host("mcp-host.example-tailnet.ts.net", Some(8445)),
                host("other.example", None),
            ]
        );
        assert_eq!(
            config.allowed_hosts[0].to_string(),
            "mcp-host.example-tailnet.ts.net:8445"
        );
        assert_eq!(
            config.allowed_hosts[1].to_string(),
            "other.example (any port)"
        );
    }

    #[test]
    fn invalid_allowed_hosts_refused() {
        let dir = TempDir::new("allowed-bad");
        for value in [
            "a.example,",
            ",a.example",
            "a.example,,b.example",
            "https://a.example",
            "a.example/mcp",
            "*.example",
            "*",
            "a .example",
            "a.example:",
            "a.example:0",
            "a.example:70000",
            "a.example:+80",
            "a.example:80:80",
            ".",
            ":8445",
            "a..example",
            ".a.example",
            "a.example..",
            "[::1]:8445",
            "a_b.example",
        ] {
            let err = load_config(
                env_from(&[(TOKEN_ENV, GOOD_TOKEN), (ALLOWED_HOSTS_ENV, value)]),
                "127.0.0.1",
                8765,
                dir.path(),
            )
            .unwrap_err();
            assert!(
                err.0
                    .starts_with("OPENAB_SHEPHERD_ALLOWED_HOSTS: invalid entry"),
                "{value}: {err}"
            );
        }
    }
}
