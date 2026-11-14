//! Who may call which tool: named clients, scopes, and the check that runs
//! before every tool call.
//!
//! A clients file (`OPENAB_SHEPHERD_CLIENTS`, JSON) lists clients. Each has an
//! id, the SHA-256 of its bearer token (the server never stores the token),
//! the scopes it holds, the launch profiles it may start, and an optional
//! expiry in Unix seconds. A token looks like `osh_<id>_<64 hex>`; the id picks
//! the record and a constant-time compare of two 32-byte digests decides.
//!
//! Scopes are an explicit list. None implies another, and there is no
//! wildcard. [`authorize`] runs before `tools::call`; a tool without a scope
//! is denied. Client policy only narrows what the server and the profile
//! already allow: `start:arbitrary` does not turn on `allow_arbitrary_command`.
//!
//! Not here yet (later phases of issue #4): pane ownership, reload without a
//! restart, `dialog:deny`, a token CLI. [`Caller::pane_policy`] is the seam
//! for the ownership work.

use crate::config::{check_outside_repo, ConfigError};
use crate::tools::{Args, ToolError};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// The path of the clients file.
pub const CLIENTS_ENV: &str = "OPENAB_SHEPHERD_CLIENTS";
/// The id of the client made from the single legacy token.
pub const LEGACY_ID: &str = "legacy";
/// Printed at startup when a legacy token runs beside a clients file.
pub const LEGACY_WARNING: &str =
    "the legacy token is accepted beside the clients file (allow_legacy_token): it holds every scope";
pub const TOKEN_PREFIX: &str = "osh_";
const SECRET_HEX_LEN: usize = 64;
const MAX_ID_LEN: usize = 32;
const MAX_TOKEN_LEN: usize = TOKEN_PREFIX.len() + MAX_ID_LEN + 1 + SECRET_HEX_LEN;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_CLIENTS: usize = 256;
const MAX_PROFILES: usize = 64;
const MAX_PROFILE_LEN: usize = 128;
/// Longest piece of a claimed id that an authentication failure reports.
pub const ID_PREFIX_LEN: usize = 16;
/// Compared against when the id is unknown, so the id's existence does not
/// show in the time a lookup takes.
const DUMMY_DIGEST: [u8; 32] = [0x5a; 32];

/// What a client may do. Nothing implies anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// `list_panes`, `get_status`, `wait_status`: metadata only.
    Observe,
    /// `read_output`: a screen can show credentials a program printed.
    Read,
    /// `send_input`, `interrupt_pane`.
    Control,
    /// `start_pane` with a `profile` on the client's allowlist.
    Start,
    /// `start_pane` with `argv` or `line` (the server must also allow it).
    StartArbitrary,
    /// `answer_dialog`.
    Dialog,
    /// `remove_pane`.
    Remove,
}

impl Scope {
    pub const ALL: [Scope; 7] = [
        Scope::Observe,
        Scope::Read,
        Scope::Control,
        Scope::Start,
        Scope::StartArbitrary,
        Scope::Dialog,
        Scope::Remove,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Observe => "observe",
            Scope::Read => "read",
            Scope::Control => "control",
            Scope::Start => "start",
            Scope::StartArbitrary => "start:arbitrary",
            Scope::Dialog => "dialog",
            Scope::Remove => "remove",
        }
    }

    pub fn parse(text: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|scope| scope.as_str() == text)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The launch profiles a client may start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileAccess {
    /// Any profile: the legacy client only.
    Any,
    /// Exactly these; an empty list allows none.
    Only(Vec<String>),
}

impl ProfileAccess {
    fn allows(&self, profile: &str) -> bool {
        match self {
            ProfileAccess::Any => true,
            ProfileAccess::Only(list) => list.iter().any(|p| p == profile),
        }
    }
}

/// One record of the clients file, or the legacy client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub id: String,
    pub token_sha256: [u8; 32],
    pub scopes: BTreeSet<Scope>,
    pub profiles: ProfileAccess,
    /// Unix seconds; the token stops working at this second.
    pub expires: Option<u64>,
}

/// Per-pane limits of a client. Empty in PR 1: pane ownership (`own` or
/// `all`) and a live-pane cap arrive here later, without a change to the
/// signature of anything that takes a [`Caller`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PanePolicy {}

/// Who is calling: the matched client, never anything the request claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    client: String,
    scopes: BTreeSet<Scope>,
    profiles: ProfileAccess,
    pane_policy: PanePolicy,
}

impl Caller {
    pub fn new(
        client: impl Into<String>,
        scopes: impl IntoIterator<Item = Scope>,
        profiles: ProfileAccess,
    ) -> Self {
        Self {
            client: client.into(),
            scopes: scopes.into_iter().collect(),
            profiles,
            pane_policy: PanePolicy::default(),
        }
    }

    /// The caller of a deployment with one token: every scope, any profile.
    pub fn legacy() -> Self {
        Self::new(LEGACY_ID, Scope::ALL, ProfileAccess::Any)
    }

    fn from_client(client: &Client) -> Self {
        Self {
            client: client.id.clone(),
            scopes: client.scopes.clone(),
            profiles: client.profiles.clone(),
            pane_policy: PanePolicy::default(),
        }
    }

    pub fn client(&self) -> &str {
        &self.client
    }

    pub fn scopes(&self) -> &BTreeSet<Scope> {
        &self.scopes
    }

    pub fn has(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }

    /// The seam for per-pane policy (ownership, limits). Nothing reads it yet.
    pub fn pane_policy(&self) -> &PanePolicy {
        &self.pane_policy
    }

    /// Whether `tools/list` should show `tool` to this caller. A convenience
    /// only: [`authorize`] is the check.
    pub fn may_list(&self, tool: &str) -> bool {
        any_scope(tool).is_some_and(|scopes| scopes.iter().any(|s| self.has(*s)))
    }
}

/// The scopes of which a caller needs at least one to use `tool` at all.
/// `None` for a tool without a mapping, which [`authorize`] denies. The
/// `start_pane` arguments narrow this further (see [`authorize`]).
pub fn any_scope(tool: &str) -> Option<&'static [Scope]> {
    Some(match tool {
        "list_panes" | "get_status" | "wait_status" => &[Scope::Observe],
        "read_output" => &[Scope::Read],
        "send_input" | "interrupt_pane" => &[Scope::Control],
        "start_pane" => &[Scope::Start, Scope::StartArbitrary],
        "answer_dialog" => &[Scope::Dialog],
        "remove_pane" => &[Scope::Remove],
        _ => return None,
    })
}

/// A refused call. Becomes a tool result with the code `forbidden`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    pub tool: String,
    /// The scope that was missing, when a scope was the reason.
    pub scope: Option<Scope>,
    pub reason: String,
}

impl Denied {
    fn scope(tool: &str, scope: Scope) -> Self {
        Self {
            tool: tool.to_string(),
            scope: Some(scope),
            reason: format!("{tool} needs the {scope} scope"),
        }
    }

    pub fn to_tool_error(&self) -> ToolError {
        let mut data = json!({"error": "forbidden", "message": self.reason});
        if let Some(scope) = self.scope {
            data["scope"] = json!(scope.as_str());
        }
        ToolError {
            text: format!("forbidden: {}", self.reason),
            data,
        }
    }
}

/// Decide whether `caller` may run `tool` with `args`. Deny by default: a
/// tool without a scope is refused.
///
/// `start_pane` needs `start` and a profile on the client's list when it
/// names a `profile`, and `start:arbitrary` when it carries `argv` or `line`
/// (a key that is present counts, even with a null value). Whether the server
/// allows arbitrary commands at all stays a separate ceiling in the tool.
pub fn authorize(caller: &Caller, tool: &str, args: &Args) -> Result<(), Denied> {
    let Some(needed) = any_scope(tool) else {
        return Err(Denied {
            tool: tool.to_string(),
            scope: None,
            reason: format!("{tool} has no scope, so it is denied"),
        });
    };
    if tool != "start_pane" {
        return match needed.iter().find(|scope| !caller.has(**scope)) {
            None => Ok(()),
            Some(missing) => Err(Denied::scope(tool, *missing)),
        };
    }
    let arbitrary = args.contains_key("argv") || args.contains_key("line");
    let profile = args.get("profile").filter(|value| !value.is_null());
    if profile.is_none() && !arbitrary && !needed.iter().any(|s| caller.has(*s)) {
        return Err(Denied::scope(tool, Scope::Start));
    }
    if let Some(profile) = profile {
        if !caller.has(Scope::Start) {
            return Err(Denied::scope(tool, Scope::Start));
        }
        let allowed = profile.as_str().is_some_and(|p| caller.profiles.allows(p));
        if !allowed {
            let shown = profile
                .as_str()
                .map(|p| p.chars().take(MAX_PROFILE_LEN).collect::<String>())
                .unwrap_or_default();
            return Err(Denied {
                tool: tool.to_string(),
                scope: None,
                reason: format!("profile {shown:?} is not on this client's list"),
            });
        }
    }
    if arbitrary && !caller.has(Scope::StartArbitrary) {
        return Err(Denied::scope(tool, Scope::StartArbitrary));
    }
    Ok(())
}

/// Why a request was not authenticated. The HTTP layer answers 401 for all of
/// them; the audit file keeps the reason and a short piece of the claimed id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthFailure {
    pub reason: &'static str,
    /// At most [`ID_PREFIX_LEN`] characters of `[a-z0-9-]` from the id the
    /// token claimed; empty when the token had no readable id. Never the secret.
    pub id_prefix: String,
}

/// The clients a server accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clients {
    clients: Vec<Client>,
    legacy: Option<Client>,
    allow_legacy_token: bool,
}

impl Clients {
    /// A deployment with the single legacy token and no clients file.
    pub fn legacy_only(token: &str) -> Self {
        Self {
            clients: Vec::new(),
            legacy: Some(legacy_client(token)),
            allow_legacy_token: false,
        }
    }

    /// Add the legacy token to a clients file's table. Refused unless the
    /// file says `"allow_legacy_token": true`: the legacy token holds every
    /// scope, so it would undo the narrower tokens.
    pub fn with_legacy_token(mut self, token: &str) -> Result<Self, ConfigError> {
        if !self.allow_legacy_token {
            return Err(ConfigError(format!(
                "a legacy token is set beside {CLIENTS_ENV}: remove it, or set \"allow_legacy_token\": true in the clients file"
            )));
        }
        self.legacy = Some(legacy_client(token));
        Ok(self)
    }

    pub fn allow_legacy_token(&self) -> bool {
        self.allow_legacy_token
    }

    pub fn has_legacy(&self) -> bool {
        self.legacy.is_some()
    }

    pub fn clients(&self) -> &[Client] {
        &self.clients
    }

    /// Read and validate the clients file. The path is resolved first, must be
    /// outside the project folder and every git repository, a regular file
    /// owned by this user, with no group or other bits (it holds hashes).
    pub fn load(path: &Path, project_dir: &Path) -> Result<Self, ConfigError> {
        let canonical = fs::canonicalize(path)
            .map_err(|err| ConfigError(format!("cannot read clients file: {err}")))?;
        check_outside_repo(&canonical, project_dir, "clients file")?;
        let mut file = fs::File::open(&canonical)
            .map_err(|err| ConfigError(format!("cannot read clients file: {err}")))?;
        let meta = file
            .metadata()
            .map_err(|err| ConfigError(format!("cannot read clients file: {err}")))?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        check_file_meta(&meta, unsafe { libc::geteuid() })?;
        let mut text = String::new();
        (&mut file)
            .take(MAX_FILE_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|err| ConfigError(format!("cannot read clients file: {err}")))?;
        Self::parse(&text)
    }

    /// Validate the whole file; nothing is used unless all of it is valid.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(err("clients file is too large"));
        }
        let root: Value = serde_json::from_str(text)
            .map_err(|e| err(&format!("clients file is not valid JSON: {e}")))?;
        let root = root
            .as_object()
            .ok_or_else(|| err("clients file must be a JSON object"))?;
        known_keys(
            root,
            &["version", "allow_legacy_token", "clients"],
            "clients file",
        )?;
        if root.get("version").and_then(Value::as_u64) != Some(1) {
            return Err(err("clients file: version must be 1"));
        }
        let allow_legacy_token = match root.get("allow_legacy_token") {
            None => false,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => return Err(err("allow_legacy_token must be true or false")),
        };
        let list = root
            .get("clients")
            .and_then(Value::as_array)
            .ok_or_else(|| err("clients file: clients must be an array"))?;
        if list.len() > MAX_CLIENTS {
            return Err(err(&format!("at most {MAX_CLIENTS} clients")));
        }
        let mut clients: Vec<Client> = Vec::new();
        for (index, item) in list.iter().enumerate() {
            let client = parse_client(item, index)?;
            if clients.iter().any(|c| c.id == client.id) {
                return Err(err(&format!("duplicate client id {:?}", client.id)));
            }
            clients.push(client);
        }
        Ok(Self {
            clients,
            legacy: None,
            allow_legacy_token,
        })
    }

    /// Authenticate an `Authorization` header value (`Bearer <token>`).
    /// `now` is Unix seconds, from the caller's clock.
    pub fn authenticate(&self, header: Option<&str>, now: u64) -> Result<Caller, AuthFailure> {
        self.check(header, now).0
    }

    /// [`Clients::authenticate`], plus how many digest compares ran, so a test
    /// can show that an unknown id costs the same as a known one.
    fn check(&self, header: Option<&str>, now: u64) -> (Result<Caller, AuthFailure>, usize) {
        let fail = |reason: &'static str, id_prefix: String| AuthFailure { reason, id_prefix };
        let Some(header) = header else {
            return (Err(fail("missing", String::new())), 0);
        };
        let Some(token) = header.strip_prefix("Bearer ") else {
            return (Err(fail("malformed", String::new())), 0);
        };
        if token.is_empty() || token.len() > MAX_TOKEN_LEN {
            return (Err(fail("malformed", String::new())), 0);
        }
        let digest = sha256(token.as_bytes());
        let claimed = parse_token(token);
        let id_prefix = claimed.map(id_prefix).unwrap_or_default();
        let record = claimed.and_then(|id| self.clients.iter().find(|c| c.id == id));
        let expected = record.map_or(&DUMMY_DIGEST, |c| &c.token_sha256);
        let mut compares = 1;
        let matched = digest_eq(&digest, expected) && record.is_some();
        if matched {
            let client = record.expect("matched implies a record");
            if client.expires.is_some_and(|expires| now >= expires) {
                return (Err(fail("expired", id_prefix)), compares);
            }
            return (Ok(Caller::from_client(client)), compares);
        }
        if let Some(legacy) = &self.legacy {
            compares += 1;
            if digest_eq(&digest, &legacy.token_sha256) {
                return (Ok(Caller::from_client(legacy)), compares);
            }
        }
        let reason = match (claimed, record) {
            (None, _) => "malformed",
            (Some(_), None) => "unknown_id",
            (Some(_), Some(_)) => "bad_secret",
        };
        (Err(fail(reason, id_prefix)), compares)
    }
}

fn legacy_client(token: &str) -> Client {
    Client {
        id: LEGACY_ID.to_string(),
        token_sha256: sha256(token.as_bytes()),
        scopes: Scope::ALL.into_iter().collect(),
        profiles: ProfileAccess::Any,
        expires: None,
    }
}

fn err(text: &str) -> ConfigError {
    ConfigError(text.to_string())
}

fn known_keys(
    object: &Map<String, Value>,
    allowed: &[&str],
    what: &str,
) -> Result<(), ConfigError> {
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        None => Ok(()),
        Some(key) => Err(ConfigError(format!(
            "{what}: unknown field {:?}",
            key.chars().take(40).collect::<String>()
        ))),
    }
}

/// The file's own rules, given the user it must belong to.
fn check_file_meta(meta: &fs::Metadata, euid: u32) -> Result<(), ConfigError> {
    if !meta.is_file() {
        return Err(err("clients file must be a regular file"));
    }
    if meta.uid() != euid {
        return Err(err(
            "clients file must be owned by the user running Shepherd",
        ));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(err(
            "clients file must not be readable or writable by group or others (chmod 600)",
        ));
    }
    Ok(())
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn parse_client(item: &Value, index: usize) -> Result<Client, ConfigError> {
    let at = |text: &str| ConfigError(format!("clients[{index}]: {text}"));
    let object = item.as_object().ok_or_else(|| at("must be an object"))?;
    known_keys(
        object,
        &["id", "token_sha256", "scopes", "profiles", "expires"],
        &format!("clients[{index}]"),
    )?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| at("id must be a string"))?;
    if !valid_id(id) {
        return Err(at(&format!(
            "id must be 1 to {MAX_ID_LEN} characters of a-z, 0-9 and '-'"
        )));
    }
    if id == LEGACY_ID {
        return Err(at("the id \"legacy\" is reserved"));
    }
    let hash = object
        .get("token_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| at("token_sha256 must be a string"))?;
    let token_sha256 = hex_decode_32(hash)
        .ok_or_else(|| at("token_sha256 must be 64 hex characters (the SHA-256 of the token)"))?;
    let scope_list = object
        .get("scopes")
        .and_then(Value::as_array)
        .ok_or_else(|| at("scopes must be an array"))?;
    let mut scopes = BTreeSet::new();
    for scope in scope_list {
        let scope = scope
            .as_str()
            .and_then(Scope::parse)
            .ok_or_else(|| at("unknown scope"))?;
        if !scopes.insert(scope) {
            return Err(at(&format!("scope {scope} is listed twice")));
        }
    }
    if scopes.is_empty() {
        return Err(at("scopes must not be empty"));
    }
    let profiles = match object.get("profiles") {
        None => Vec::new(),
        Some(Value::Array(list)) => {
            if list.len() > MAX_PROFILES {
                return Err(at(&format!("at most {MAX_PROFILES} profiles")));
            }
            let mut names: Vec<String> = Vec::new();
            for name in list {
                let name = name
                    .as_str()
                    .filter(|n| {
                        !n.is_empty()
                            && n.len() <= MAX_PROFILE_LEN
                            && n.chars().all(|c| !c.is_control() && !c.is_whitespace())
                    })
                    .ok_or_else(|| {
                        at("a profile name must be a non-empty string without spaces")
                    })?;
                if !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
            }
            names
        }
        Some(_) => return Err(at("profiles must be an array")),
    };
    let expires = match object.get("expires") {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| at("expires must be Unix seconds, an integer"))?,
        ),
    };
    Ok(Client {
        id: id.to_string(),
        token_sha256,
        scopes,
        profiles: ProfileAccess::Only(profiles),
        expires,
    })
}

/// The id of a token shaped `osh_<id>_<64 hex>`, or `None`.
fn parse_token(token: &str) -> Option<&str> {
    let rest = token.strip_prefix(TOKEN_PREFIX)?;
    let (id, secret) = rest.rsplit_once('_')?;
    let secret_ok = secret.len() == SECRET_HEX_LEN && secret.bytes().all(|b| b.is_ascii_hexdigit());
    (secret_ok && valid_id(id)).then_some(id)
}

fn id_prefix(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .take(ID_PREFIX_LEN)
        .collect()
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&sha256(bytes))
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode_32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        *slot = (high * 16 + low) as u8;
    }
    Some(out)
}

/// Compare two digests without stopping at the first difference. Both are
/// 32 bytes, so no length leaks either.
fn digest_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Unix seconds now.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{git_init, TempDir};
    use crate::tools::TOOL_NAMES;

    const SECRET_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SECRET_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn token(id: &str, secret: &str) -> String {
        format!("osh_{id}_{secret}")
    }

    fn file_text(clients: Value) -> String {
        json!({"version": 1, "clients": clients}).to_string()
    }

    fn client_json(id: &str, secret: &str, scopes: &[&str]) -> Value {
        json!({
            "id": id,
            "token_sha256": sha256_hex(token(id, secret).as_bytes()),
            "scopes": scopes,
            "profiles": ["ci"],
        })
    }

    fn table() -> Clients {
        let mut ci = client_json("ci-1", SECRET_A, &["observe", "start"]);
        ci["expires"] = json!(1000);
        Clients::parse(&file_text(json!([
            ci,
            client_json("alice", SECRET_B, &["observe", "read", "control"]),
        ])))
        .unwrap()
    }

    fn bearer(token: &str) -> String {
        format!("Bearer {token}")
    }

    fn args(value: Value) -> Args {
        value.as_object().unwrap().clone()
    }

    fn only(scope: Scope, profiles: &[&str]) -> Caller {
        Caller::new(
            "t",
            [scope],
            ProfileAccess::Only(profiles.iter().map(|p| p.to_string()).collect()),
        )
    }

    fn sample_args(tool: &str) -> Args {
        match tool {
            "start_pane" => args(json!({"profile": "ci"})),
            "send_input" => args(json!({"terminal": "w", "text": "x"})),
            "answer_dialog" => args(json!({"terminal": "w", "choice": "allow_once"})),
            _ => args(json!({"terminal": "w"})),
        }
    }

    // ---- scopes ----

    #[test]
    fn every_tool_has_a_scope_mapping() {
        for tool in TOOL_NAMES {
            assert!(
                any_scope(tool).is_some(),
                "tool {tool} has no scope: add it to clients::any_scope"
            );
        }
        assert!(any_scope("not_a_tool").is_none());
        assert!(any_scope("").is_none());
    }

    #[test]
    fn scope_names_round_trip() {
        for scope in Scope::ALL {
            assert_eq!(Scope::parse(scope.as_str()), Some(scope));
        }
        assert_eq!(Scope::parse("*"), None);
        assert_eq!(Scope::parse("admin"), None);
    }

    #[test]
    fn every_tool_by_every_scope() {
        for tool in TOOL_NAMES {
            for scope in Scope::ALL {
                let caller = only(scope, &["ci"]);
                let allowed = authorize(&caller, tool, &sample_args(tool)).is_ok();
                let expected = match tool {
                    "list_panes" | "get_status" | "wait_status" => scope == Scope::Observe,
                    "read_output" => scope == Scope::Read,
                    "send_input" | "interrupt_pane" => scope == Scope::Control,
                    "start_pane" => scope == Scope::Start,
                    "answer_dialog" => scope == Scope::Dialog,
                    "remove_pane" => scope == Scope::Remove,
                    other => panic!("unmapped tool {other}"),
                };
                assert_eq!(allowed, expected, "{tool} with only {scope}");
            }
        }
    }

    #[test]
    fn a_caller_without_scopes_may_do_nothing() {
        let nobody = Caller::new("n", [], ProfileAccess::Any);
        for tool in TOOL_NAMES {
            assert!(
                authorize(&nobody, tool, &sample_args(tool)).is_err(),
                "{tool}"
            );
            assert!(!nobody.may_list(tool), "{tool}");
        }
    }

    #[test]
    fn unknown_tool_is_denied_even_for_the_legacy_caller() {
        let denied = authorize(&Caller::legacy(), "format_disk", &Args::new()).unwrap_err();
        assert_eq!(denied.scope, None);
        assert_eq!(denied.to_tool_error().data["error"], "forbidden");
    }

    #[test]
    fn legacy_caller_holds_every_scope_and_any_profile() {
        let legacy = Caller::legacy();
        assert_eq!(legacy.client(), "legacy");
        for tool in TOOL_NAMES {
            assert!(
                authorize(&legacy, tool, &sample_args(tool)).is_ok(),
                "{tool}"
            );
        }
        let arbitrary = args(json!({"argv": ["ls"]}));
        assert!(authorize(&legacy, "start_pane", &arbitrary).is_ok());
        let other = args(json!({"profile": "anything"}));
        assert!(authorize(&legacy, "start_pane", &other).is_ok());
    }

    #[test]
    fn profile_must_be_on_the_clients_list() {
        let caller = only(Scope::Start, &["ci"]);
        assert!(authorize(&caller, "start_pane", &args(json!({"profile": "ci"}))).is_ok());
        for bad in [
            json!("review"),
            json!("CI"),
            json!(" ci"),
            json!(7),
            json!(["ci"]),
        ] {
            let denied =
                authorize(&caller, "start_pane", &args(json!({"profile": bad}))).unwrap_err();
            assert!(denied.reason.contains("not on this client's list"), "{bad}");
        }
    }

    #[test]
    fn empty_profile_list_allows_no_profile() {
        let caller = only(Scope::Start, &[]);
        assert!(authorize(&caller, "start_pane", &args(json!({"profile": "ci"}))).is_err());
    }

    #[test]
    fn argv_and_line_need_start_arbitrary_even_with_start() {
        let start = only(Scope::Start, &["ci"]);
        for body in [
            json!({"argv": ["sh"]}),
            json!({"line": "ls"}),
            json!({"argv": null}),
            json!({"profile": "ci", "argv": ["sh"]}),
            json!({"profile": null, "line": "ls"}),
        ] {
            let denied = authorize(&start, "start_pane", &args(body.clone())).unwrap_err();
            assert!(
                denied.reason.contains("start:arbitrary") || denied.scope.is_some(),
                "{body}"
            );
        }
        let arbitrary = only(Scope::StartArbitrary, &[]);
        assert!(authorize(&arbitrary, "start_pane", &args(json!({"argv": ["ls"]}))).is_ok());
        assert!(authorize(&arbitrary, "start_pane", &args(json!({"line": "ls"}))).is_ok());
        // start:arbitrary does not grant profiles.
        assert!(authorize(&arbitrary, "start_pane", &args(json!({"profile": "ci"}))).is_err());
        // A profile plus argv needs both.
        let both = Caller::new(
            "b",
            [Scope::Start, Scope::StartArbitrary],
            ProfileAccess::Only(vec!["ci".into()]),
        );
        let mixed = args(json!({"profile": "ci", "argv": ["ls"]}));
        assert!(authorize(&both, "start_pane", &mixed).is_ok());
    }

    #[test]
    fn denial_becomes_a_forbidden_tool_error() {
        let denied = authorize(
            &only(Scope::Observe, &[]),
            "answer_dialog",
            &sample_args("answer_dialog"),
        )
        .unwrap_err();
        let error = denied.to_tool_error();
        assert_eq!(error.data["error"], "forbidden");
        assert_eq!(error.data["scope"], "dialog");
        assert!(error.text.starts_with("forbidden: "));
    }

    #[test]
    fn may_list_follows_the_scopes() {
        let caller = only(Scope::StartArbitrary, &[]);
        assert!(caller.may_list("start_pane"));
        assert!(!caller.may_list("answer_dialog"));
        assert!(!caller.may_list("nope"));
        assert_eq!(caller.pane_policy(), &PanePolicy::default());
    }

    // ---- authenticate ----

    #[test]
    fn authenticates_a_known_token() {
        let caller = table()
            .authenticate(Some(&bearer(&token("alice", SECRET_B))), 5)
            .unwrap();
        assert_eq!(caller.client(), "alice");
        assert!(caller.has(Scope::Read) && !caller.has(Scope::Dialog));
    }

    #[test]
    fn rejects_bad_requests_with_a_reason() {
        let t = table();
        let fail = |header: Option<&str>| t.authenticate(header, 5).unwrap_err();
        assert_eq!(fail(None).reason, "missing");
        assert_eq!(fail(Some("Basic abc")).reason, "malformed");
        assert_eq!(fail(Some("bearer abc")).reason, "malformed");
        assert_eq!(fail(Some("Bearer ")).reason, "malformed");
        assert_eq!(fail(Some("Bearer not-a-token")).reason, "malformed");
        let unknown = fail(Some(&bearer(&token("mallory", SECRET_A))));
        assert_eq!(
            (unknown.reason, unknown.id_prefix.as_str()),
            ("unknown_id", "mallory")
        );
        let wrong = fail(Some(&bearer(&token("alice", SECRET_A))));
        assert_eq!(
            (wrong.reason, wrong.id_prefix.as_str()),
            ("bad_secret", "alice")
        );
        let long = "x".repeat(MAX_TOKEN_LEN + 1);
        assert_eq!(fail(Some(&bearer(&long))).reason, "malformed");
    }

    #[test]
    fn expiry_is_checked_against_the_injected_clock() {
        let t = table();
        let header = bearer(&token("ci-1", SECRET_A));
        assert!(t.authenticate(Some(&header), 999).is_ok());
        let late = t.authenticate(Some(&header), 1000).unwrap_err();
        assert_eq!((late.reason, late.id_prefix.as_str()), ("expired", "ci-1"));
        assert!(t.authenticate(Some(&header), 5000).is_err());
    }

    #[test]
    fn unknown_id_costs_the_same_compares_as_a_known_one() {
        let t = table();
        let (_, known) = t.check(Some(&bearer(&token("alice", SECRET_A))), 5);
        let (_, unknown) = t.check(Some(&bearer(&token("mallory", SECRET_A))), 5);
        assert_eq!(known, 1);
        assert_eq!(unknown, known);
        let with_legacy = Clients::parse(&file_text(json!([client_json(
            "a",
            SECRET_A,
            &["observe"]
        )])))
        .unwrap();
        let with_legacy = Clients {
            allow_legacy_token: true,
            ..with_legacy
        }
        .with_legacy_token("a-long-legacy-token-value-0123456789")
        .unwrap();
        let (_, a) = with_legacy.check(Some(&bearer(&token("a", SECRET_B))), 5);
        let (_, b) = with_legacy.check(Some(&bearer(&token("zz", SECRET_B))), 5);
        assert_eq!(a, b);
    }

    #[test]
    fn id_prefix_is_filtered_and_capped() {
        let t = table();
        let id = format!("{}-{}", "a".repeat(20), "b");
        let failure = t
            .authenticate(Some(&bearer(&token(&id, SECRET_A))), 0)
            .unwrap_err();
        assert!(failure.id_prefix.len() <= ID_PREFIX_LEN);
        assert!(failure.id_prefix.chars().all(|c| c == 'a' || c == '-'));
        // A token that is not osh-shaped reports no id at all.
        let junk = t.authenticate(Some("Bearer Zm9v\"quote"), 0).unwrap_err();
        assert_eq!(junk.id_prefix, "");
    }

    #[test]
    fn the_secret_never_appears_in_a_failure() {
        let t = table();
        let failure = t
            .authenticate(Some(&bearer(&token("alice", SECRET_A))), 0)
            .unwrap_err();
        assert!(!format!("{failure:?}").contains(SECRET_A));
    }

    #[test]
    fn legacy_token_authenticates_as_legacy_with_every_scope() {
        let t = Clients::legacy_only("a-long-legacy-token-value-0123456789");
        let caller = t
            .authenticate(Some("Bearer a-long-legacy-token-value-0123456789"), 0)
            .unwrap();
        assert_eq!(caller, Caller::legacy());
        assert!(t
            .authenticate(Some("Bearer a-long-legacy-token-value-012345678"), 0)
            .is_err());
        assert!(t
            .authenticate(Some("Bearer a-long-legacy-token-value-01234567890"), 0)
            .is_err());
        assert!(t.authenticate(Some("Bearer "), 0).is_err());
    }

    #[test]
    fn a_table_without_legacy_refuses_a_legacy_looking_token() {
        let failure = table()
            .authenticate(Some("Bearer a-long-legacy-token-value-0123456789"), 0)
            .unwrap_err();
        assert_eq!(failure.reason, "malformed");
    }

    // ---- parse ----

    fn parse_err(text: &str) -> String {
        Clients::parse(text).unwrap_err().0
    }

    #[test]
    fn parses_a_valid_file() {
        let t = table();
        assert_eq!(t.clients().len(), 2);
        let ci = &t.clients()[0];
        assert_eq!(ci.expires, Some(1000));
        assert_eq!(ci.profiles, ProfileAccess::Only(vec!["ci".to_string()]));
        assert!(!t.allow_legacy_token() && !t.has_legacy());
    }

    #[test]
    fn rejects_unknown_fields_everywhere() {
        let good = client_json("a", SECRET_A, &["observe"]);
        assert!(
            parse_err(&json!({"version": 1, "clients": [], "extra": 1}).to_string())
                .contains("unknown field")
        );
        let mut bad = good.clone();
        bad["token"] = json!("osh_x");
        assert!(parse_err(&file_text(json!([bad]))).contains("unknown field"));
    }

    #[test]
    fn rejects_duplicate_ids() {
        let a = client_json("a", SECRET_A, &["observe"]);
        assert!(parse_err(&file_text(json!([a.clone(), a]))).contains("duplicate"));
    }

    #[test]
    fn rejects_bad_ids() {
        for id in [
            "",
            "Alice",
            "a_b",
            "a b",
            "a.b",
            "é",
            "a".repeat(33).as_str(),
            "legacy",
        ] {
            let mut c = client_json("a", SECRET_A, &["observe"]);
            c["id"] = json!(id);
            assert!(Clients::parse(&file_text(json!([c]))).is_err(), "{id:?}");
        }
        assert!(valid_id("ci-1") && valid_id("0"));
    }

    #[test]
    fn rejects_bad_hashes() {
        for hash in [
            json!(""),
            json!("abc"),
            json!("g".repeat(64)),
            json!("a".repeat(63)),
            json!("a".repeat(65)),
            json!(5),
        ] {
            let mut c = client_json("a", SECRET_A, &["observe"]);
            c["token_sha256"] = hash.clone();
            assert!(Clients::parse(&file_text(json!([c]))).is_err(), "{hash}");
        }
        // Upper-case hex is the same digest.
        let mut c = client_json("a", SECRET_A, &["observe"]);
        let upper = c["token_sha256"].as_str().unwrap().to_uppercase();
        c["token_sha256"] = json!(upper);
        let t = Clients::parse(&file_text(json!([c]))).unwrap();
        assert!(t
            .authenticate(Some(&bearer(&token("a", SECRET_A))), 0)
            .is_ok());
    }

    #[test]
    fn rejects_bad_scopes_expiry_profiles_and_version() {
        let base = || client_json("a", SECRET_A, &["observe"]);
        let mutate = |key: &str, value: Value| {
            let mut c = base();
            c[key] = value;
            Clients::parse(&file_text(json!([c]))).is_err()
        };
        assert!(mutate("scopes", json!(["observe", "observe"])));
        assert!(mutate("scopes", json!(["*"])));
        assert!(mutate("scopes", json!([])));
        assert!(mutate("scopes", json!("observe")));
        assert!(mutate("expires", json!(-1)));
        assert!(mutate("expires", json!("2030-01-01")));
        assert!(mutate("expires", json!(1.5)));
        assert!(mutate("profiles", json!("ci")));
        assert!(mutate("profiles", json!([""])));
        assert!(mutate("profiles", json!(["a b"])));
        assert!(parse_err(&json!({"version": 2, "clients": []}).to_string()).contains("version"));
        assert!(parse_err("{}").contains("version"));
        assert!(parse_err("[]").contains("object"));
        assert!(parse_err("nope").contains("JSON"));
        assert!(parse_err(&json!({"version": 1, "clients": {}}).to_string()).contains("array"));
        assert!(parse_err(
            &json!({"version": 1, "clients": [], "allow_legacy_token": "yes"}).to_string()
        )
        .contains("allow_legacy_token"));
    }

    #[test]
    fn profiles_may_be_left_out_and_duplicates_collapse() {
        let mut c = client_json("a", SECRET_A, &["start"]);
        c.as_object_mut().unwrap().remove("profiles");
        let t = Clients::parse(&file_text(json!([c]))).unwrap();
        assert_eq!(t.clients()[0].profiles, ProfileAccess::Only(vec![]));
        let mut c = client_json("a", SECRET_A, &["start"]);
        c["profiles"] = json!(["x", "x"]);
        let t = Clients::parse(&file_text(json!([c]))).unwrap();
        assert_eq!(
            t.clients()[0].profiles,
            ProfileAccess::Only(vec!["x".to_string()])
        );
    }

    #[test]
    fn too_many_clients_is_refused() {
        let list: Vec<Value> = (0..=MAX_CLIENTS)
            .map(|i| client_json(&format!("c{i}"), SECRET_A, &["observe"]))
            .collect();
        assert!(parse_err(&file_text(json!(list))).contains("at most"));
    }

    // ---- legacy beside a clients file ----

    #[test]
    fn legacy_token_beside_a_clients_file_needs_the_flag() {
        let plain = table();
        let refused = plain.with_legacy_token("a-long-legacy-token-value-0123456789");
        assert!(refused.unwrap_err().0.contains("allow_legacy_token"));
        let allowed = Clients::parse(
            &json!({"version": 1, "allow_legacy_token": true, "clients": []}).to_string(),
        )
        .unwrap();
        assert!(allowed.allow_legacy_token());
        let merged = allowed
            .with_legacy_token("a-long-legacy-token-value-0123456789")
            .unwrap();
        assert!(merged.has_legacy());
        assert!(merged
            .authenticate(Some("Bearer a-long-legacy-token-value-0123456789"), 0)
            .is_ok());
    }

    // ---- load ----

    fn write_file(dir: &Path, name: &str, text: &str, mode: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn load_accepts_a_strict_file_outside_any_repo() {
        let dir = TempDir::new("clients-ok");
        let project = TempDir::new("clients-project");
        let path = write_file(dir.path(), "clients.json", &file_text(json!([])), 0o600);
        let t = Clients::load(&path, project.path()).unwrap();
        assert!(t.clients().is_empty());
        // 0400 is stricter and fine.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        assert!(Clients::load(&path, project.path()).is_ok());
    }

    #[test]
    fn load_refuses_group_or_other_bits() {
        let dir = TempDir::new("clients-mode");
        let project = TempDir::new("clients-project");
        for mode in [0o640, 0o604, 0o660, 0o644, 0o602] {
            let path = write_file(dir.path(), "c.json", &file_text(json!([])), mode);
            let message = Clients::load(&path, project.path()).unwrap_err().0;
            assert!(message.contains("chmod 600"), "{mode:o}: {message}");
        }
    }

    #[test]
    fn load_refuses_a_file_owned_by_someone_else() {
        let dir = TempDir::new("clients-owner");
        let path = write_file(dir.path(), "c.json", &file_text(json!([])), 0o600);
        let meta = fs::metadata(&path).unwrap();
        let someone_else = meta.uid().wrapping_add(1);
        assert!(check_file_meta(&meta, someone_else)
            .unwrap_err()
            .0
            .contains("owned by"));
        assert!(check_file_meta(&meta, meta.uid()).is_ok());
    }

    #[test]
    fn load_refuses_directories_and_missing_files() {
        let dir = TempDir::new("clients-kind");
        let project = TempDir::new("clients-project");
        assert!(Clients::load(dir.path(), project.path()).is_err());
        assert!(Clients::load(&dir.path().join("missing"), project.path()).is_err());
    }

    #[test]
    fn load_refuses_a_path_inside_a_git_repo() {
        let repo = TempDir::new("clients-repo");
        git_init(repo.path());
        let project = TempDir::new("clients-project");
        let path = write_file(repo.path(), "c.json", &file_text(json!([])), 0o600);
        let message = Clients::load(&path, project.path()).unwrap_err().0;
        assert!(message.contains("outside the git repository"), "{message}");
        // A symlink from outside into the repo is resolved first.
        let outside = TempDir::new("clients-link");
        let link = outside.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(Clients::load(&link, project.path()).is_err());
    }

    #[test]
    fn load_refuses_an_unknown_field_and_a_duplicate_id() {
        let dir = TempDir::new("clients-bad");
        let project = TempDir::new("clients-project");
        let path = write_file(
            dir.path(),
            "a.json",
            &json!({"version": 1, "clients": [], "oops": true}).to_string(),
            0o600,
        );
        assert!(Clients::load(&path, project.path())
            .unwrap_err()
            .0
            .contains("unknown field"));
        let a = client_json("a", SECRET_A, &["observe"]);
        let path = write_file(
            dir.path(),
            "b.json",
            &file_text(json!([a.clone(), a])),
            0o600,
        );
        assert!(Clients::load(&path, project.path())
            .unwrap_err()
            .0
            .contains("duplicate"));
    }

    #[test]
    fn load_refuses_an_oversized_file() {
        let dir = TempDir::new("clients-big");
        let project = TempDir::new("clients-project");
        let big = format!(
            "{{\"version\":1,\"clients\":[],\"pad\":\"{}\"}}",
            "x".repeat(2_000_000)
        );
        let path = write_file(dir.path(), "big.json", &big, 0o600);
        assert!(Clients::load(&path, project.path()).is_err());
    }

    // ---- digest helpers ----

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hex_round_trips_and_digest_eq_sees_every_byte() {
        let digest = sha256(b"x");
        assert_eq!(hex_decode_32(&hex_encode(&digest)), Some(digest));
        for index in 0..32 {
            let mut other = digest;
            other[index] ^= 1;
            assert!(!digest_eq(&digest, &other), "byte {index}");
        }
        assert!(digest_eq(&digest, &digest));
    }
}
