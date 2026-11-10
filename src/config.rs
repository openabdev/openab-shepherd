//! Startup checks: bearer token, bind address, and the herdr-core socket path.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub token: String,
    pub socket_path: PathBuf,
    pub bind: SocketAddr,
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
    let socket_path = load_socket_path(&lookup)?;
    Ok(Config {
        token,
        socket_path,
        bind: SocketAddr::new(ip, port),
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
    check_outside_repo(&canonical, project_dir)?;
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

/// Refuse a token file that is inside the project folder, or inside any git
/// repository that the project folder or the token's own folder belongs to.
/// Repository roots come from `git rev-parse --show-toplevel`, not from a path prefix.
fn check_outside_repo(token: &Path, project_dir: &Path) -> Result<(), ConfigError> {
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
        return Err(ConfigError(
            "token file must be outside the git repository and the project folder".to_string(),
        ));
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
            env_from(&[(TOKEN_ENV, GOOD_TOKEN), (SOCKET_ENV, SOCKET)]),
            "127.0.0.1",
            8765,
            dir.path(),
        )
        .unwrap();
        assert_eq!(config.token, GOOD_TOKEN);
        assert_eq!(config.socket_path, PathBuf::from(SOCKET));
        assert_eq!(config.bind, "127.0.0.1:8765".parse().unwrap());
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
            env_from(&[(TOKEN_ENV, GOOD_TOKEN)]),
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
}
