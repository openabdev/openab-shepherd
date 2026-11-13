//! Operator-defined launch profiles: what `start_pane` may start.
//!
//! A profile fixes the program (`argv`, then `extra_args`), the working
//! directories a caller may pick from, and the environment. Credentials reach
//! a child only through a profile's `env`, read from their source when the
//! pane starts; a caller-chosen command (allowed only with
//! `allow_arbitrary_command`) never gets one. See README "Launch profiles".
//!
//! The file is JSON, named by `OPENAB_SHEPHERD_PROFILES`, and must be outside
//! every git repository the token file must be outside. It must be a regular
//! file owned by Shepherd's user and writable by no one else. Without it there
//! are no profiles and no arbitrary commands, so `start_pane` starts nothing.
//!
//! A profile's program is found when a pane starts, in Shepherd's `PATH`
//! using only absolute entries, so the directory a caller picks never decides
//! which executable gets the profile's environment.

use crate::agent::{Agent, PermissionMode};
use crate::backend::Secret;
use crate::config::{check_outside_repo, ConfigError};
use crate::input::{validate_input, validate_name};
use serde_json::{json, Map, Value};
use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// The path of the launch profiles file.
pub const PROFILES_ENV: &str = "OPENAB_SHEPHERD_PROFILES";
/// What reads a `keychain:` value.
pub const SECURITY: &str = "/usr/bin/security";
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);
const KEYCHAIN_POLL: Duration = Duration::from_millis(20);
const KEYCHAIN_MAX_BYTES: u64 = 64 * 1024;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PROFILES: usize = 64;
const MAX_ARGV: usize = 256;
const MAX_CWDS: usize = 32;
/// The native backend's limit on extra variables.
const MAX_ENV: usize = 32;
const SECURITY_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
/// The `PATH` a profile's program is looked up in when Shepherd has none
/// (the native backend gives such a child the same one).
const DEFAULT_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
/// Names a profile's `env` may not set besides Shepherd's own: `TERM` (fixed
/// by the backend), `PATH` (it decides what a child's commands run) and
/// loader variables.
const PROTECTED_KEYS: [&str; 2] = ["TERM", "PATH"];
const PROTECTED_PREFIXES: [&str; 2] = ["DYLD_", "LD_"];

type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Where a profile variable's value comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvSource {
    /// `keychain:<service>`: `security find-generic-password -s <service> -w`
    /// at launch (macOS only).
    Keychain(String),
    /// `env:<NAME>`: Shepherd's own variable, at launch.
    Env(String),
    /// Any other string, as written. For non-secret settings only: it sits in
    /// the file and shows in a `Debug` of the profile.
    Literal(String),
}

/// One launch profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    /// The fixed `argv` followed by the fixed `extra_args`.
    pub argv: Vec<String>,
    /// Canonical directories; a pane's cwd is one of them or below one.
    pub cwds: Vec<PathBuf>,
    pub env: Vec<(String, EnvSource)>,
    /// Which agent runs; decides the permission dialog answer_dialog reads.
    pub agent: Agent,
    pub permission_mode: PermissionMode,
    /// Lets a kiro profile declare `permission_mode` bypass. Ignored for devin.
    pub allow_bypass: bool,
}

impl Profile {
    /// Arguments Shepherd inserts right after the program. A devin profile gets
    /// `--permission-mode` with the value for its mode, so the mode never
    /// depends on a config file. A kiro profile gets none: its arguments are the
    /// profile's own `argv`.
    pub fn permission_args(&self) -> Vec<String> {
        match self.agent {
            Agent::Kiro | Agent::OpenCode => Vec::new(),
            Agent::Devin => vec![
                "--permission-mode".to_string(),
                self.permission_mode.devin_value().to_string(),
            ],
        }
    }
}

/// Why a requested working directory was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CwdError {
    NotFound,
    Invalid(String),
}

/// The launch profiles and whether caller-chosen commands are allowed.
#[derive(Clone)]
pub struct LaunchPolicy {
    /// `start_pane` may take `argv` or `line`; such a pane never gets a
    /// profile's environment.
    pub allow_arbitrary_command: bool,
    profiles: Vec<Profile>,
    security: PathBuf,
    env: EnvLookup,
}

impl Default for LaunchPolicy {
    /// No profiles and no arbitrary commands.
    fn default() -> Self {
        Self {
            allow_arbitrary_command: false,
            profiles: Vec::new(),
            security: PathBuf::from(SECURITY),
            env: Arc::new(|key| std::env::var(key).ok()),
        }
    }
}

impl fmt::Debug for LaunchPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchPolicy")
            .field("allow_arbitrary_command", &self.allow_arbitrary_command)
            .field("profiles", &self.profiles)
            .finish_non_exhaustive()
    }
}

/// Names a profile may not set and `env:` may not copy: Shepherd's own
/// configuration (token, token file, profiles path, backend). The `HC_` prefix
/// stays reserved from the removed hc backend.
fn is_shepherd_own(name: &str) -> bool {
    name.starts_with("OPENAB_SHEPHERD_") || name.starts_with("HC_")
}

fn is_env_name(name: &str) -> bool {
    name.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn err(message: impl Into<String>) -> ConfigError {
    ConfigError(format!("launch profiles: {}", message.into()))
}

/// The profiles file is policy: a regular file owned by Shepherd's effective
/// user that neither group nor others can write.
fn check_file(is_file: bool, uid: u32, mode: u32, euid: u32) -> Result<(), ConfigError> {
    if !is_file {
        return Err(err("the file must be a regular file"));
    }
    if uid != euid {
        return Err(err("the file must be owned by the user Shepherd runs as"));
    }
    if mode & 0o022 != 0 {
        return Err(err(
            "the file must not be writable by group or others (chmod 600 or 644)",
        ));
    }
    Ok(())
}

impl LaunchPolicy {
    /// Read the file `OPENAB_SHEPHERD_PROFILES` names, or the default policy
    /// when it is unset. `project_dir` is the folder Shepherd runs from.
    pub fn load<F>(lookup: F, project_dir: &Path) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let Some(path) = lookup(PROFILES_ENV).filter(|p| !p.trim().is_empty()) else {
            return Ok(Self::default());
        };
        let canonical =
            fs::canonicalize(path.trim()).map_err(|e| err(format!("cannot read the file: {e}")))?;
        check_outside_repo(&canonical, project_dir, "the launch profiles file")?;
        // Check and read the same open file, so the path cannot be swapped
        // between the two.
        let file =
            fs::File::open(&canonical).map_err(|e| err(format!("cannot read the file: {e}")))?;
        let meta = file
            .metadata()
            .map_err(|e| err(format!("cannot read the file: {e}")))?;
        let euid = crate::native::effective_uid();
        check_file(meta.is_file(), meta.uid(), meta.permissions().mode(), euid)?;
        if meta.len() > MAX_FILE_BYTES {
            return Err(err(format!(
                "the file is larger than {MAX_FILE_BYTES} bytes"
            )));
        }
        let mut text = String::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|e| err(format!("cannot read the file: {e}")))?;
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(err(format!(
                "the file is larger than {MAX_FILE_BYTES} bytes"
            )));
        }
        Self::from_json(&text)
    }

    /// The executable a profile's pane runs: `argv[0]` when it is absolute,
    /// else the first executable regular file of that name in Shepherd's
    /// `PATH`, skipping empty and relative entries so the pane's working
    /// directory never takes part. The directory is canonical; the file name
    /// is kept, so a program that looks at its own name still sees it.
    pub fn program(&self, profile: &Profile) -> Result<PathBuf, String> {
        let name = &profile.argv[0];
        if Path::new(name).is_absolute() {
            return Ok(PathBuf::from(name));
        }
        let path = (self.env)("PATH")
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| DEFAULT_PATH.to_string());
        for dir in path.split(':').map(Path::new) {
            if !dir.is_absolute() {
                continue;
            }
            let Ok(dir) = fs::canonicalize(dir) else {
                continue;
            };
            let candidate = dir.join(name);
            let runnable = fs::metadata(&candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
            if runnable {
                return Ok(candidate);
            }
        }
        Err(format!(
            "launch profile {}: {name} is not in an absolute directory of Shepherd's PATH",
            profile.name
        ))
    }

    /// Parse and check profiles from JSON text (see README "Launch profiles").
    /// Every `cwds` entry must exist now.
    pub fn from_json(text: &str) -> Result<Self, ConfigError> {
        parse(text, cfg!(target_os = "macos"))
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    /// What a caller may start: profile names and allowed directories, and
    /// whether arbitrary commands are allowed. No environment, no argv.
    pub fn to_json(&self) -> Value {
        let profiles: Vec<Value> = self
            .profiles
            .iter()
            .map(|p| {
                let cwds: Vec<String> = p
                    .cwds
                    .iter()
                    .map(|c| c.to_string_lossy().into_owned())
                    .collect();
                json!({"name": p.name, "cwds": cwds})
            })
            .collect();
        json!({"profiles": profiles, "allow_arbitrary_command": self.allow_arbitrary_command})
    }

    /// The pane's working directory: `requested` after resolving symlinks and
    /// `..`, when it is one of the profile's directories or below one, else
    /// the profile's first directory when nothing was requested.
    pub fn cwd(&self, profile: &Profile, requested: Option<&str>) -> Result<PathBuf, CwdError> {
        let Some(requested) = requested else {
            return Ok(profile.cwds[0].clone());
        };
        if !Path::new(requested).is_absolute() {
            return Err(CwdError::Invalid("must be an absolute path".to_string()));
        }
        let canonical = match fs::canonicalize(requested) {
            Ok(path) => path,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(CwdError::NotFound),
            Err(_) => return Err(CwdError::Invalid("cannot be resolved".to_string())),
        };
        if !canonical.is_dir() {
            return Err(CwdError::Invalid("must be a directory".to_string()));
        }
        if profile
            .cwds
            .iter()
            .any(|allowed| canonical.starts_with(allowed))
        {
            Ok(canonical)
        } else {
            Err(CwdError::Invalid(format!(
                "outside the directories launch profile {} allows",
                profile.name
            )))
        }
    }

    /// The profile's environment, each value read from its source now. An
    /// error names the variable and its source, never a value.
    pub fn resolve_env(&self, profile: &Profile) -> Result<Vec<(String, Secret)>, String> {
        let mut out = Vec::with_capacity(profile.env.len());
        for (key, source) in &profile.env {
            let fail = |why: String| format!("launch profile {}: {key}: {why}", profile.name);
            let value = match source {
                EnvSource::Literal(value) => Secret::new(value.clone()),
                EnvSource::Env(name) => match (self.env)(name).filter(|v| !v.is_empty()) {
                    Some(value) if !value.contains('\0') => Secret::new(value),
                    Some(_) => return Err(fail(format!("env:{name} contains NUL"))),
                    None => {
                        return Err(fail(format!(
                            "env:{name} is not set in Shepherd's environment"
                        )))
                    }
                },
                EnvSource::Keychain(service) => self
                    .read_keychain(service)
                    .map_err(|why| fail(format!("keychain:{service}: {why}")))?,
            };
            out.push((key.clone(), value));
        }
        Ok(out)
    }

    /// `security find-generic-password -s <service> -w`, with a clean
    /// environment, no stdin, stderr discarded, and a 10 s limit.
    fn read_keychain(&self, service: &str) -> Result<Secret, String> {
        let mut command = Command::new(&self.security);
        command
            .args(["find-generic-password", "-s", service, "-w"])
            .env_clear()
            .env("PATH", SECURITY_PATH)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Shepherd's threads block SIGINT/TERM/HUP for its signal handler;
        // the reader starts with the default mask and dispositions.
        crate::native::default_signals_on_exec(&mut command);
        for key in ["HOME", "USER"] {
            if let Some(value) = (self.env)(key).filter(|v| !v.is_empty()) {
                command.env(key, value);
            }
        }
        let started = {
            // A pane spawning now must not leak its PTY into this child.
            let _lock = crate::native::SPAWN_LOCK
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            command.spawn()
        };
        let mut child =
            started.map_err(|e| format!("cannot run {}: {e}", self.security.display()))?;
        let deadline = Instant::now() + KEYCHAIN_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(KEYCHAIN_POLL),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "the read did not finish in {} s",
                        KEYCHAIN_TIMEOUT.as_secs()
                    ));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("waiting for the read failed: {e}"));
                }
            }
        };
        let mut bytes = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            stdout
                .take(KEYCHAIN_MAX_BYTES)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("reading the value failed: {e}"))?;
        }
        if !status.success() {
            return Err(match status.code() {
                Some(code) => format!("no readable keychain item (security exit {code})"),
                None => "the read was killed by a signal".to_string(),
            });
        }
        let mut value =
            String::from_utf8(bytes).map_err(|_| "the value is not UTF-8".to_string())?;
        if value.ends_with('\n') {
            value.pop();
        }
        if value.is_empty() || value.contains('\0') {
            return Err("the value is empty or contains NUL".to_string());
        }
        Ok(Secret::new(value))
    }

    /// Caller-chosen commands allowed, no profiles (tests).
    #[cfg(test)]
    pub(crate) fn arbitrary() -> Self {
        Self {
            allow_arbitrary_command: true,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn with_security(mut self, path: impl Into<PathBuf>) -> Self {
        self.security = path.into();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_env_lookup(
        mut self,
        lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.env = Arc::new(lookup);
        self
    }
}

fn parse(text: &str, keychain_ok: bool) -> Result<LaunchPolicy, ConfigError> {
    let root: Value =
        serde_json::from_str(text).map_err(|e| err(format!("not valid JSON: {e}")))?;
    let root = root
        .as_object()
        .ok_or_else(|| err("the file must hold a JSON object"))?;
    only_keys(root, &["allow_arbitrary_command", "profiles"], "the file")?;
    let allow_arbitrary_command = match root.get("allow_arbitrary_command") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(err("allow_arbitrary_command must be true or false")),
    };
    let items = match root.get("profiles") {
        None => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => return Err(err("profiles must be a list")),
    };
    if items.len() > MAX_PROFILES {
        return Err(err(format!("at most {MAX_PROFILES} profiles are allowed")));
    }
    let mut profiles: Vec<Profile> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let profile = parse_profile(item, index, keychain_ok)?;
        if profiles.iter().any(|p| p.name == profile.name) {
            return Err(err(format!("two profiles are named {}", profile.name)));
        }
        profiles.push(profile);
    }
    Ok(LaunchPolicy {
        allow_arbitrary_command,
        profiles,
        ..LaunchPolicy::default()
    })
}

fn only_keys(map: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), ConfigError> {
    match map.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(err(format!("{what}: unknown field {key:?}"))),
        None => Ok(()),
    }
}

fn strings(value: Option<&Value>, what: &str) -> Result<Vec<String>, ConfigError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| err(format!("{what} must be a list of strings")))?;
    items
        .iter()
        .map(|item| {
            let text = item
                .as_str()
                .ok_or_else(|| err(format!("{what} must be a list of strings")))?;
            validate_input(text).map_err(|e| err(format!("{what}: {e}")))
        })
        .collect()
}

fn parse_profile(item: &Value, index: usize, keychain_ok: bool) -> Result<Profile, ConfigError> {
    let map = item
        .as_object()
        .ok_or_else(|| err(format!("profile {index} must be an object")))?;
    let name = map
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| err(format!("profile {index} needs a name")))?;
    let name = validate_name(name).map_err(|e| err(format!("profile {index}: {e}")))?;
    let at = format!("profile {name}");
    only_keys(
        map,
        &[
            "name",
            "agent",
            "permission_mode",
            "allow_bypass",
            "argv",
            "extra_args",
            "cwds",
            "env",
        ],
        &at,
    )?;

    let mut argv = strings(map.get("argv"), &format!("{at}: argv"))?;
    if argv.is_empty() {
        return Err(err(format!("{at}: argv must be a non-empty list")));
    }
    if argv[0].contains('/') && !Path::new(&argv[0]).is_absolute() {
        return Err(err(format!(
            "{at}: argv[0] must be a program name or an absolute path"
        )));
    }
    argv.extend(strings(
        map.get("extra_args"),
        &format!("{at}: extra_args"),
    )?);
    if argv.len() > MAX_ARGV {
        return Err(err(format!(
            "{at}: argv and extra_args have more than {MAX_ARGV} elements"
        )));
    }

    let agent = match map.get("agent") {
        None => Agent::Kiro,
        Some(Value::String(text)) => Agent::parse(text)
            .ok_or_else(|| err(format!("{at}: agent must be kiro, devin or opencode")))?,
        Some(_) => return Err(err(format!("{at}: agent must be kiro, devin or opencode"))),
    };
    let permission_mode = match map.get("permission_mode") {
        None => PermissionMode::Normal,
        Some(Value::String(text)) => PermissionMode::parse(text).ok_or_else(|| {
            err(format!(
                "{at}: permission_mode must be normal, accept-edits or bypass"
            ))
        })?,
        Some(_) => {
            return Err(err(format!(
                "{at}: permission_mode must be normal, accept-edits or bypass"
            )))
        }
    };
    let allow_bypass = match map.get("allow_bypass") {
        None => false,
        Some(Value::Bool(allow)) => *allow,
        Some(_) => return Err(err(format!("{at}: allow_bypass must be true or false"))),
    };
    // The argv decides whether this is OpenCode, not only the declared agent: a profile
    // with no `agent` field still defaults to Kiro, and a wrapper such as `sh -c` or `npx`
    // runs OpenCode from a later element.
    let runs_opencode = argv.iter().any(|a| names_opencode(a));
    if agent == Agent::OpenCode || runs_opencode {
        if let Some(arg) = argv.iter().find(|a| is_opencode_auto_flag(a)) {
            return Err(err(format!(
                "{at}: argv and extra_args may not pass {arg} for opencode: OpenCode auto-approve flags are refused"
            )));
        }
    }
    match (agent, permission_mode) {
        (Agent::Kiro, PermissionMode::AcceptEdits) => {
            return Err(err(format!(
                "{at}: permission_mode accept-edits is not supported for kiro: no Kiro mode was measured"
            )))
        }
        (Agent::Kiro, PermissionMode::Bypass) if !allow_bypass => {
            return Err(err(format!(
                "{at}: permission_mode bypass for kiro needs \"allow_bypass\": true"
            )))
        }
        (Agent::OpenCode, PermissionMode::AcceptEdits | PermissionMode::Bypass) => {
            return Err(err(format!(
                "{at}: permission_mode must be normal for opencode: no OpenCode permission mode was measured"
            )))
        }
        (Agent::Devin, _)
            if argv
                .iter()
                .any(|a| a == "--permission-mode" || a.starts_with("--permission-mode=")) =>
        {
            return Err(err(format!(
                "{at}: argv and extra_args may not set --permission-mode for devin; use permission_mode"
            )))
        }
        (Agent::Devin, _)
            if argv.iter().any(|a| {
                a == "--respect-workspace-trust" || a.starts_with("--respect-workspace-trust=")
            }) =>
        {
            return Err(err(format!(
                "{at}: argv and extra_args may not set --respect-workspace-trust for devin: the trust check stays on, and a new directory must be trusted in Devin first"
            )))
        }
        _ => {}
    }

    let dirs = strings(map.get("cwds"), &format!("{at}: cwds"))?;
    if dirs.is_empty() || dirs.len() > MAX_CWDS {
        return Err(err(format!(
            "{at}: cwds must list 1 to {MAX_CWDS} directories"
        )));
    }
    let mut cwds = Vec::with_capacity(dirs.len());
    for dir in dirs {
        if !Path::new(&dir).is_absolute() {
            return Err(err(format!("{at}: cwds: {dir} is not an absolute path")));
        }
        let canonical = fs::canonicalize(&dir)
            .ok()
            .filter(|path| path.is_dir())
            .ok_or_else(|| err(format!("{at}: cwds: {dir} is not an existing directory")))?;
        cwds.push(canonical);
    }

    let env_map = match map.get("env") {
        None => Map::new(),
        Some(Value::Object(env)) => env.clone(),
        Some(_) => {
            return Err(err(format!(
                "{at}: env must be an object of names to strings"
            )))
        }
    };
    if env_map.len() > MAX_ENV {
        return Err(err(format!("{at}: env has more than {MAX_ENV} variables")));
    }
    let mut env = Vec::with_capacity(env_map.len());
    for (key, value) in &env_map {
        if !is_env_name(key) {
            return Err(err(format!(
                "{at}: env: a name must match [A-Z_][A-Z0-9_]*"
            )));
        }
        if agent == Agent::Devin && key == "DEVIN_PERMISSION_MODE" {
            return Err(err(format!(
                "{at}: env may not set DEVIN_PERMISSION_MODE for devin; use permission_mode"
            )));
        }
        if PROTECTED_KEYS.contains(&key.as_str())
            || PROTECTED_PREFIXES.iter().any(|p| key.starts_with(p))
            || is_shepherd_own(key)
        {
            return Err(err(format!("{at}: env may not set {key}")));
        }
        let text = value
            .as_str()
            .ok_or_else(|| err(format!("{at}: env: {key} must be a string")))?;
        env.push((key.clone(), parse_source(text, &at, key, keychain_ok)?));
    }
    Ok(Profile {
        name,
        argv,
        cwds,
        env,
        agent,
        permission_mode,
        allow_bypass,
    })
}

/// True for an argument that contains an OpenCode auto-approve spelling. The
/// match is a substring match on the whole argument, so `--auto`, `--auto=true`,
/// `--auto-…`, `--config=--auto`, and a shell string such as `sh -c "opencode --auto"`
/// are all refused. It also matches `autoaccept` or `auto-accept` anywhere, such as
/// `session.permissions=autoaccept`. Over-refusal is the safe direction here.
fn is_opencode_auto_flag(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    lower.contains("--auto") || lower.contains("autoaccept") || lower.contains("auto-accept")
}

/// True when an argument contains `opencode`, ignoring case. A substring match catches
/// `OpenCode`, `npx opencode`, `sh -c "true;opencode --auto"` and paths. Over-refusal is
/// the safe direction.
fn names_opencode(arg: &str) -> bool {
    arg.to_ascii_lowercase().contains("opencode")
}

fn parse_source(
    text: &str,
    at: &str,
    key: &str,
    keychain_ok: bool,
) -> Result<EnvSource, ConfigError> {
    if let Some(service) = text.strip_prefix("keychain:") {
        if !keychain_ok {
            return Err(err(format!(
                "{at}: env: {key}: keychain: sources work only on macOS"
            )));
        }
        let service = validate_input(service)
            .map_err(|e| err(format!("{at}: env: {key}: keychain service: {e}")))?;
        return Ok(EnvSource::Keychain(service));
    }
    if let Some(name) = text.strip_prefix("env:") {
        if !is_env_name(name) || is_shepherd_own(name) {
            return Err(err(format!(
                "{at}: env: {key}: env:{name} must name a variable [A-Z_][A-Z0-9_]* that is not Shepherd's own"
            )));
        }
        return Ok(EnvSource::Env(name.to_string()));
    }
    if text.chars().any(|c| c.is_control() && c != '\t') {
        return Err(err(format!(
            "{at}: env: {key}: a value may not contain control characters"
        )));
    }
    Ok(EnvSource::Literal(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{git_init, TempDir};
    use std::os::unix::fs::PermissionsExt;

    fn policy(text: &str) -> LaunchPolicy {
        parse(text, true).unwrap()
    }

    fn refused(text: &str) -> String {
        parse(text, true).unwrap_err().0
    }

    fn one_profile(dir: &Path, env: Value) -> String {
        json!({"profiles": [{"name": "kiro", "argv": ["kiro-cli", "chat"],
            "extra_args": ["--trust-workspace"], "cwds": [dir], "env": env}]})
        .to_string()
    }

    #[test]
    fn a_profile_parses_with_its_fixed_argv_and_canonical_cwds() {
        let dir = TempDir::new("profiles-parse");
        let p = policy(&one_profile(
            dir.path(),
            json!({"KIRO_API_KEY": "keychain:openab-shepherd-kiro-api-key",
                   "FROM_ENV": "env:SOME_VAR", "MODE": "plain"}),
        ));
        assert!(!p.allow_arbitrary_command);
        let kiro = p.profile("kiro").unwrap();
        assert_eq!(kiro.argv, ["kiro-cli", "chat", "--trust-workspace"]);
        assert_eq!(kiro.cwds, [fs::canonicalize(dir.path()).unwrap()]);
        assert_eq!(
            kiro.env,
            [
                (
                    "FROM_ENV".to_string(),
                    EnvSource::Env("SOME_VAR".to_string())
                ),
                (
                    "KIRO_API_KEY".to_string(),
                    EnvSource::Keychain("openab-shepherd-kiro-api-key".to_string())
                ),
                ("MODE".to_string(), EnvSource::Literal("plain".to_string())),
            ]
        );
        assert!(p.profile("other").is_none());
        // What callers see: names and directories, no argv and no env.
        let shown = p.to_json().to_string();
        assert!(shown.contains("\"kiro\""), "{shown}");
        assert!(!shown.contains("kiro-cli"), "{shown}");
        assert!(!shown.contains("KIRO_API_KEY"), "{shown}");
        assert!(!shown.contains("keychain"), "{shown}");
    }

    #[test]
    fn an_empty_or_missing_file_setting_starts_nothing() {
        let p = LaunchPolicy::load(|_| None, Path::new("/tmp")).unwrap();
        assert!(!p.allow_arbitrary_command);
        assert_eq!(p.to_json()["profiles"], json!([]));
        assert!(policy("{}").profile("kiro").is_none());
        assert!(policy(r#"{"allow_arbitrary_command": true}"#).allow_arbitrary_command);
    }

    #[test]
    fn bad_profiles_are_refused() {
        let dir = TempDir::new("profiles-bad");
        let d = dir.path().to_string_lossy().into_owned();
        let cases = [
            ("[]".to_string(), "JSON object"),
            ("{".to_string(), "not valid JSON"),
            (r#"{"profile": []}"#.to_string(), "unknown field"),
            (r#"{"allow_arbitrary_command": "yes"}"#.to_string(), "true or false"),
            (json!({"profiles": [{"argv": ["a"], "cwds": [d]}]}).to_string(), "needs a name"),
            (json!({"profiles": [{"name": "Bad Name", "argv": ["a"], "cwds": [d]}]}).to_string(), "name must be"),
            (json!({"profiles": [{"name": "p", "argv": [], "cwds": [d]}]}).to_string(), "non-empty"),
            (json!({"profiles": [{"name": "p", "argv": ["a\u{1b}"], "cwds": [d]}]}).to_string(), "control character"),
            (json!({"profiles": [{"name": "p", "argv": ["a"], "cwds": []}]}).to_string(), "cwds must list"),
            (json!({"profiles": [{"name": "p", "argv": ["a"], "cwds": ["rel/dir"]}]}).to_string(), "absolute"),
            (json!({"profiles": [{"name": "p", "argv": ["a"], "cwds": ["/shepherd-no-such-dir"]}]}).to_string(), "existing directory"),
            (json!({"profiles": [{"name": "p", "argv": ["a"], "cwds": [d], "shell": true}]}).to_string(), "unknown field"),
            (json!({"profiles": [{"name": "p", "argv": ["a"], "cwds": [d]}, {"name": "p", "argv": ["b"], "cwds": [d]}]}).to_string(), "two profiles"),
            (one_profile(dir.path(), json!({"lower": "x"})), "must match"),
            (one_profile(dir.path(), json!({"TERM": "dumb"})), "may not set TERM"),
            (one_profile(dir.path(), json!({"OPENAB_SHEPHERD_TOKEN": "x"})), "may not set"),
            (one_profile(dir.path(), json!({"HC_SOCKET": "x"})), "may not set"),
            (one_profile(dir.path(), json!({"A": "env:OPENAB_SHEPHERD_TOKEN"})), "not Shepherd's own"),
            (one_profile(dir.path(), json!({"A": "env:HC_SOCKET"})), "not Shepherd's own"),
            (one_profile(dir.path(), json!({"A": "env:lower"})), "must name a variable"),
            (one_profile(dir.path(), json!({"A": "keychain:"})), "keychain service"),
            (one_profile(dir.path(), json!({"A": 3})), "must be a string"),
            (one_profile(dir.path(), json!({"A": "a\nb"})), "control characters"),
        ];
        for (text, want) in cases {
            let message = refused(&text);
            assert!(message.contains(want), "{text}: {message}");
        }
        let many_args: Vec<String> = (0..MAX_ARGV).map(|i| i.to_string()).collect();
        let text = json!({"profiles": [{"name": "p", "argv": ["a"], "extra_args": many_args, "cwds": [d]}]});
        assert!(refused(&text.to_string()).contains("more than"));
    }

    #[test]
    fn agent_and_permission_mode_default_to_kiro_and_normal() {
        let dir = TempDir::new("profiles-agent-default");
        let d = dir.path();
        let p = policy(
            &json!({"profiles": [
                {"name": "k", "argv": ["kiro-cli"], "cwds": [d]},
                {"name": "dn", "argv": ["devin"], "cwds": [d], "agent": "devin"},
                {"name": "da", "argv": ["devin"], "cwds": [d], "agent": "devin", "permission_mode": "accept-edits"},
                {"name": "db", "argv": ["devin"], "cwds": [d], "agent": "devin", "permission_mode": "bypass"},
                {"name": "kb", "argv": ["kiro-cli"], "cwds": [d], "permission_mode": "bypass", "allow_bypass": true},
            ]})
            .to_string(),
        );
        let kiro = p.profile("k").unwrap();
        assert_eq!(kiro.agent, Agent::Kiro);
        assert_eq!(kiro.permission_mode, PermissionMode::Normal);
        assert!(!kiro.allow_bypass);
        assert!(kiro.permission_args().is_empty());
        assert_eq!(
            p.profile("kb").unwrap().permission_mode,
            PermissionMode::Bypass
        );
        assert!(p.profile("kb").unwrap().permission_args().is_empty());

        let devin = p.profile("dn").unwrap();
        assert_eq!(devin.agent, Agent::Devin);
        assert_eq!(devin.permission_mode, PermissionMode::Normal);
        assert_eq!(devin.permission_args(), ["--permission-mode", "auto"]);
        assert_eq!(
            p.profile("da").unwrap().permission_args(),
            ["--permission-mode", "accept-edits"]
        );
        assert_eq!(
            p.profile("db").unwrap().permission_args(),
            ["--permission-mode", "dangerous"]
        );
    }

    #[test]
    fn agent_and_permission_mode_refusals() {
        let dir = TempDir::new("profiles-agent-bad");
        let d = dir.path().to_string_lossy().into_owned();
        let profile = |extra: Value| {
            let mut map = json!({"name": "p", "argv": ["x"], "cwds": [d]});
            for (key, value) in extra.as_object().unwrap() {
                map[key] = value.clone();
            }
            json!({"profiles": [map]}).to_string()
        };
        let cases = [
            (
                json!({"agent": "open-code"}),
                "agent must be kiro, devin or opencode",
            ),
            (json!({"agent": 1}), "agent must be kiro, devin or opencode"),
            (
                json!({"agent": "opencode", "permission_mode": "accept-edits"}),
                "permission_mode must be normal for opencode",
            ),
            (
                json!({"agent": "opencode", "permission_mode": "bypass", "allow_bypass": true}),
                "permission_mode must be normal for opencode",
            ),
            (
                json!({"permission_mode": "yolo"}),
                "permission_mode must be",
            ),
            (json!({"permission_mode": true}), "permission_mode must be"),
            (
                json!({"allow_bypass": "yes"}),
                "allow_bypass must be true or false",
            ),
            (
                json!({"permission_mode": "accept-edits"}),
                "accept-edits is not supported for kiro",
            ),
            (
                json!({"permission_mode": "bypass"}),
                "needs \"allow_bypass\": true",
            ),
            (
                json!({"agent": "devin", "argv": ["devin", "--permission-mode", "dangerous"]}),
                "may not set --permission-mode for devin",
            ),
            (
                json!({"agent": "devin", "extra_args": ["--permission-mode=dangerous"]}),
                "may not set --permission-mode for devin",
            ),
            (
                json!({"agent": "devin", "extra_args": ["--respect-workspace-trust", "false"]}),
                "may not set --respect-workspace-trust for devin",
            ),
            (
                json!({"agent": "devin", "argv": ["devin", "--respect-workspace-trust=false"]}),
                "may not set --respect-workspace-trust for devin",
            ),
            (
                json!({"agent": "devin", "env": {"DEVIN_PERMISSION_MODE": "dangerous"}}),
                "may not set DEVIN_PERMISSION_MODE for devin",
            ),
            (json!({"allow_bypasss": true}), "unknown field"),
        ];
        for (extra, want) in cases {
            let message = refused(&profile(extra.clone()));
            assert!(message.contains(want), "{extra}: {message}");
        }
    }

    #[test]
    fn an_opencode_profile_is_accepted_in_normal_mode_with_no_mode_flag() {
        let dir = TempDir::new("profiles-opencode-ok");
        let d = dir.path().to_string_lossy().into_owned();
        let text = json!({"profiles": [
            {"name": "oc", "argv": ["opencode"], "cwds": [d], "agent": "opencode"},
        ]})
        .to_string();
        let policy = parse(&text, true).unwrap();
        let profile = policy.profile("oc").unwrap();
        assert_eq!(profile.agent, Agent::OpenCode);
        assert_eq!(profile.permission_mode, PermissionMode::Normal);
        assert!(profile.permission_args().is_empty());
    }

    #[test]
    fn an_opencode_profile_refuses_every_auto_approve_spelling_in_argv_or_extra_args() {
        let dir = TempDir::new("profiles-opencode-auto");
        let d = dir.path().to_string_lossy().into_owned();
        let with_argv = |argv: Value| {
            json!({"profiles": [{"name": "oc", "agent": "opencode", "argv": argv, "cwds": [d]}]})
                .to_string()
        };
        for flag in [
            "--auto",
            "--auto=true",
            "--autoaccept",
            "--auto-accept",
            "--auto-approve",
            "session.permissions=autoaccept",
            "--config=session.permissions=AutoAccept",
            "--config=--auto",
        ] {
            let message = refused(&with_argv(json!(["opencode", "--standalone", flag])));
            assert!(
                message.contains(flag) && message.contains("opencode"),
                "{flag}: {message}"
            );
        }
        // A shell string that starts OpenCode with the flag is still an argv containing it.
        let shell = with_argv(json!(["sh", "-c", "opencode --auto"]));
        let message = refused(&shell);
        assert!(
            message.contains("--auto") && message.contains("opencode"),
            "{message}"
        );
        let in_extra = json!({"profiles": [{"name": "oc", "agent": "opencode",
            "argv": ["opencode"], "extra_args": ["--auto"], "cwds": [d]}]})
        .to_string();
        assert!(refused(&in_extra).contains("--auto"));
        // A normal argv passes, and a value that is only the word "auto" is not a flag.
        let normal = with_argv(json!(["opencode", "--standalone", "--model", "auto"]));
        let policy = parse(&normal, true).unwrap();
        assert_eq!(
            policy.profile("oc").unwrap().argv,
            ["opencode", "--standalone", "--model", "auto"]
        );
    }

    #[test]
    fn an_opencode_program_is_refused_auto_flags_even_without_an_agent_field() {
        let dir = TempDir::new("profiles-opencode-program");
        let d = dir.path().to_string_lossy().into_owned();
        let no_agent =
            json!({"profiles": [{"name": "oc", "argv": ["opencode", "--auto"], "cwds": [d]}]})
                .to_string();
        let message = refused(&no_agent);
        assert!(
            message.contains("--auto") && message.contains("opencode"),
            "{message}"
        );
        let absolute = json!({"profiles": [{"name": "oc", "argv": ["/usr/local/bin/opencode", "--auto"], "cwds": [d]}]})
            .to_string();
        assert!(refused(&absolute).contains("--auto"));
        // Wrappers that start OpenCode are refused even with no `agent` field.
        for argv in [
            json!(["sh", "-c", "opencode --auto"]),
            json!(["npx", "opencode", "--auto"]),
            json!(["env", "FOO=1", "/usr/local/bin/opencode", "--auto-accept"]),
        ] {
            let text = json!({"profiles": [{"name": "oc", "argv": argv, "cwds": [d]}]}).to_string();
            let message = refused(&text);
            assert!(message.contains("opencode"), "{message}");
        }
        // Case, shell separators and wrappers do not hide OpenCode.
        for argv in [
            json!(["sh", "-c", "true;opencode --auto"]),
            json!(["OpenCode", "--auto"]),
            json!(["npx", "opencode", "--auto"]),
            json!(["opencode", "--auto"]),
        ] {
            let text = json!({"profiles": [{"name": "oc", "argv": argv, "cwds": [d]}]}).to_string();
            assert!(refused(&text).contains("--auto"));
        }
        let normal = json!({"profiles": [{"name": "oc", "argv": ["opencode", "--standalone"], "cwds": [d]}]})
            .to_string();
        assert!(parse(&normal, true).is_ok());
        let auto_path = json!({"profiles": [{"name": "k", "argv": ["/opt/auto/kiro-cli", "acp"], "cwds": [d]}]})
            .to_string();
        assert!(parse(&auto_path, true).is_ok());
        // The same flag on a program that is not OpenCode is the operator's own choice.
        let kiro =
            json!({"profiles": [{"name": "k", "argv": ["kiro-cli", "--auto"], "cwds": [d]}]})
                .to_string();
        assert!(parse(&kiro, true).is_ok());
    }

    #[test]
    fn bypass_is_accepted_for_devin_only_when_set_explicitly_or_with_allow_bypass_for_kiro() {
        let dir = TempDir::new("profiles-bypass-ok");
        let d = dir.path().to_string_lossy().into_owned();
        let text = json!({"profiles": [
            {"name": "db", "argv": ["devin"], "cwds": [d], "agent": "devin", "permission_mode": "bypass"},
            {"name": "kb", "argv": ["kiro-cli"], "cwds": [d], "permission_mode": "bypass", "allow_bypass": true},
            {"name": "ka", "argv": ["kiro-cli"], "cwds": [d], "permission_mode": "normal", "allow_bypass": false},
        ]})
        .to_string();
        assert!(parse(&text, true).is_ok());
    }

    #[test]
    fn keychain_sources_are_refused_off_macos() {
        let dir = TempDir::new("profiles-keychain-os");
        let text = one_profile(dir.path(), json!({"KIRO_API_KEY": "keychain:svc"}));
        let message = parse(&text, false).unwrap_err().0;
        assert!(message.contains("only on macOS"), "{message}");
        assert!(parse(&one_profile(dir.path(), json!({"A": "env:B"})), false).is_ok());
    }

    #[test]
    fn the_file_must_be_outside_a_git_repository() {
        let base = TempDir::new("profiles-repo");
        let repo = base.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git_init(&repo);
        let inside = repo.join("profiles.json");
        fs::write(&inside, "{}").unwrap();
        let inside_text = inside.to_string_lossy().into_owned();
        let lookup = |key: &str| (key == PROFILES_ENV).then(|| inside_text.clone());
        let message = LaunchPolicy::load(lookup, &repo).unwrap_err().0;
        assert!(message.contains("outside the git repository"), "{message}");

        let outside = TempDir::new("profiles-outside");
        let file = outside.path().join("profiles.json");
        fs::write(&file, r#"{"allow_arbitrary_command": true}"#).unwrap();
        let file_text = file.to_string_lossy().into_owned();
        let lookup = |key: &str| (key == PROFILES_ENV).then(|| file_text.clone());
        assert!(
            LaunchPolicy::load(lookup, &repo)
                .unwrap()
                .allow_arbitrary_command
        );
    }

    #[test]
    fn a_cwd_must_resolve_inside_an_allowed_directory() {
        let base = TempDir::new("profiles-cwd");
        let allowed = base.path().join("work");
        let sub = allowed.join("sub");
        let outside = base.path().join("elsewhere");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, allowed.join("escape")).unwrap();
        std::os::unix::fs::symlink(&sub, outside.join("into-sub")).unwrap();
        fs::write(allowed.join("file"), "x").unwrap();
        let p = policy(&one_profile(&allowed, json!({})));
        let kiro = p.profile("kiro").unwrap();
        let canon = |path: &Path| fs::canonicalize(path).unwrap();
        let s = |path: PathBuf| path.to_string_lossy().into_owned();

        assert_eq!(p.cwd(kiro, None).unwrap(), canon(&allowed));
        assert_eq!(
            p.cwd(kiro, Some(&s(allowed.clone()))).unwrap(),
            canon(&allowed)
        );
        assert_eq!(p.cwd(kiro, Some(&s(sub.clone()))).unwrap(), canon(&sub));
        // A link elsewhere that points inside is inside.
        assert_eq!(
            p.cwd(kiro, Some(&s(outside.join("into-sub")))).unwrap(),
            canon(&sub)
        );
        for bad in [
            s(outside.clone()),
            s(allowed.join("escape")),
            s(allowed.join("..")),
            s(sub.join("../../elsewhere")),
            "/".to_string(),
        ] {
            assert!(
                matches!(p.cwd(kiro, Some(&bad)), Err(CwdError::Invalid(ref m)) if m.contains("outside")),
                "{bad}"
            );
        }
        assert!(matches!(
            p.cwd(kiro, Some("work/sub")),
            Err(CwdError::Invalid(_))
        ));
        assert!(matches!(
            p.cwd(kiro, Some(&s(allowed.join("file")))),
            Err(CwdError::Invalid(_))
        ));
        assert_eq!(
            p.cwd(kiro, Some(&s(allowed.join("missing")))),
            Err(CwdError::NotFound)
        );
    }

    #[test]
    fn env_sources_are_read_at_launch() {
        let dir = TempDir::new("profiles-env");
        let p = policy(&one_profile(
            dir.path(),
            json!({"KEY_FROM_ENV": "env:SOURCE_VAR", "LIT": "plain"}),
        ))
        .with_env_lookup(|key| (key == "SOURCE_VAR").then(|| "from-env-value".to_string()));
        let env = p.resolve_env(p.profile("kiro").unwrap()).unwrap();
        assert_eq!(
            env,
            [
                ("KEY_FROM_ENV".to_string(), Secret::new("from-env-value")),
                ("LIT".to_string(), Secret::new("plain")),
            ]
        );
        assert!(!format!("{env:?}").contains("from-env-value"));
        let unset = p.clone().with_env_lookup(|_| None);
        let message = unset
            .resolve_env(unset.profile("kiro").unwrap())
            .unwrap_err();
        assert!(message.contains("env:SOURCE_VAR is not set"), "{message}");
    }

    /// A fake `security` that records its arguments and prints `value`, or
    /// exits `code` without output.
    fn fake_security(dir: &Path, value: &str, code: i32) -> PathBuf {
        let script = dir.join("security");
        let record = dir.join("args");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nenv > '{}'\nif [ {code} -ne 0 ]; then exit {code}; fi\nprintf '%s\\n' '{value}'\n",
                record.display(),
                dir.join("env").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn keychain_sources_run_security_with_the_service_name() {
        let dir = TempDir::new("profiles-keychain");
        let fake = fake_security(dir.path(), "keychain-secret-value", 0);
        let p = policy(&one_profile(
            dir.path(),
            json!({"KIRO_API_KEY": "keychain:openab-shepherd-kiro-api-key"}),
        ))
        .with_security(&fake)
        .with_env_lookup(|key| match key {
            "HOME" => Some("/home/test".to_string()),
            "OPENAB_SHEPHERD_TOKEN" => Some("token-secret".to_string()),
            _ => None,
        });
        let env = p.resolve_env(p.profile("kiro").unwrap()).unwrap();
        assert_eq!(
            env,
            [(
                "KIRO_API_KEY".to_string(),
                Secret::new("keychain-secret-value")
            )]
        );
        assert!(!format!("{env:?}").contains("keychain-secret-value"));
        let args = fs::read_to_string(dir.path().join("args")).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                "find-generic-password",
                "-s",
                "openab-shepherd-kiro-api-key",
                "-w"
            ]
        );
        let seen = fs::read_to_string(dir.path().join("env")).unwrap();
        assert!(seen.contains("HOME=/home/test"), "{seen}");
        assert!(!seen.contains("token-secret"), "{seen}");

        // A missing item: an error naming the service and exit code only.
        let missing = fake_security(dir.path(), "never-printed", 44);
        let p = p.with_security(&missing);
        let message = p.resolve_env(p.profile("kiro").unwrap()).unwrap_err();
        assert!(
            message.contains("keychain:openab-shepherd-kiro-api-key"),
            "{message}"
        );
        assert!(message.contains("exit 44"), "{message}");
        assert!(!message.contains("never-printed"), "{message}");
    }

    #[test]
    fn the_file_must_be_the_users_and_writable_by_no_one_else() {
        // Review SOL2-B2.
        let dir = TempDir::new("profiles-perm");
        let file = dir.path().join("profiles.json");
        fs::write(&file, r#"{"allow_arbitrary_command": true}"#).unwrap();
        let text = file.to_string_lossy().into_owned();
        let project = TempDir::new("profiles-perm-project");
        let load = || {
            let text = text.clone();
            LaunchPolicy::load(
                move |key| (key == PROFILES_ENV).then(|| text.clone()),
                project.path(),
            )
        };
        for mode in [0o600, 0o644, 0o400] {
            fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
            assert!(load().unwrap().allow_arbitrary_command, "{mode:o}");
        }
        for mode in [0o620, 0o602, 0o666] {
            fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
            let message = load().unwrap_err().0;
            assert!(message.contains("writable by group or others"), "{mode:o}");
        }
        // Ownership cannot be changed without root here; the rule itself:
        assert!(check_file(true, 501, 0o100600, 501).is_ok());
        let message = check_file(true, 0, 0o100600, 501).unwrap_err().0;
        assert!(message.contains("owned by the user"), "{message}");
        let message = check_file(false, 501, 0o040700, 501).unwrap_err().0;
        assert!(message.contains("regular file"), "{message}");
        let folder = dir.path().to_string_lossy().into_owned();
        let message = LaunchPolicy::load(
            move |key| (key == PROFILES_ENV).then(|| folder.clone()),
            project.path(),
        )
        .unwrap_err()
        .0;
        assert!(message.contains("regular file"), "{message}");
    }

    #[test]
    fn argv0_is_a_name_or_an_absolute_path_and_env_may_not_set_path_or_loaders() {
        let dir = TempDir::new("profiles-argv0");
        let d = dir.path().to_string_lossy().into_owned();
        for bad in ["./kiro-cli", "bin/kiro-cli", "../x"] {
            let text = json!({"profiles": [{"name": "p", "argv": [bad], "cwds": [d]}]});
            assert!(
                refused(&text.to_string()).contains("argv[0] must be"),
                "{bad}"
            );
        }
        for key in ["PATH", "LD_PRELOAD", "DYLD_INSERT_LIBRARIES"] {
            let message = refused(&one_profile(dir.path(), json!({key: "x"})));
            assert!(message.contains("may not set"), "{key}: {message}");
        }
    }

    #[test]
    fn a_profile_program_is_found_only_in_absolute_path_entries() {
        // Review SOL2-B1: the caller's cwd never takes part.
        let dir = TempDir::new("profiles-program");
        let work = dir.path().join("work");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&bin).unwrap();
        for program in [work.join("kiro-cli"), bin.join("kiro-cli")] {
            fs::write(&program, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Not executable: skipped.
        let plain = dir.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        fs::write(plain.join("kiro-cli"), "x").unwrap();
        fs::set_permissions(plain.join("kiro-cli"), fs::Permissions::from_mode(0o644)).unwrap();
        let p = policy(&one_profile(&work, json!({})));
        let with_path = |path: String| p.clone().with_env_lookup(move |_| Some(path.clone()));
        let kiro = p.profile("kiro").unwrap();
        let found = with_path(format!(".::work:{}:{}", plain.display(), bin.display()))
            .program(kiro)
            .unwrap();
        assert_eq!(found, fs::canonicalize(&bin).unwrap().join("kiro-cli"));
        let message = with_path(".::work".to_string()).program(kiro).unwrap_err();
        assert!(message.contains("absolute directory"), "{message}");
        // An absolute argv[0] is used as written.
        let abs = policy(
            &json!({"profiles": [{"name": "sh", "argv": ["/bin/sh"],
            "cwds": [work]}]})
            .to_string(),
        );
        assert_eq!(
            abs.program(abs.profile("sh").unwrap()).unwrap(),
            PathBuf::from("/bin/sh")
        );
    }

    #[test]
    fn the_default_keychain_reader_is_usr_bin_security() {
        assert_eq!(
            LaunchPolicy::default().security,
            PathBuf::from("/usr/bin/security")
        );
    }
}
