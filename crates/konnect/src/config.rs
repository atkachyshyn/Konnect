use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Path to the kicad-cli binary
    #[serde(default = "default_kicad_cli")]
    pub kicad_cli: String,

    /// Path to the KiCAD binary (for launching the UI)
    #[serde(default = "default_kicad_binary")]
    pub kicad_binary: String,

    /// Default project directory
    #[serde(default)]
    pub project_dir: Option<PathBuf>,

    /// KiCAD IPC socket path (NNG). Auto-detected from KICAD_API_SOCKET env var if empty.
    #[serde(default = "default_ipc_address")]
    #[serde(alias = "ipc_socket_path")]
    pub ipc_address: String,

    /// MCP server transport mode
    #[serde(default)]
    pub transport: TransportMode,

    /// HTTP server bind address (used when transport includes HTTP)
    #[serde(default = "default_http_address")]
    pub http_address: String,

    /// JLCPCB database cache path
    #[serde(default)]
    pub jlcpcb_db_path: Option<PathBuf>,

    /// Log level (error, warn, info, debug, trace)
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Auto-load a tool's toolset on call instead of returning
    /// `toolset_not_loaded`. Off by default: toolsets accumulate monotonically
    /// once loaded, so auto-load trades one recoverable error for permanent
    /// context growth -- opt in only if that trade is worth it for your client.
    #[serde(default)]
    pub auto_load_toolsets: bool,

    /// Pre-load every toolset at startup so the very first `tools/list` is
    /// complete. Off by default: a full listing costs roughly 25K tokens
    /// against the ~2K baseline, which is the whole reason the router exists.
    ///
    /// Turn it on for an MCP client that caches the initial tool list and does
    /// not act on `notifications/tools/list_changed`. For those clients a tool
    /// missing from the first listing can never be called at all --
    /// `load_toolset` reports the names it loaded but returns no schemas, so
    /// there is nothing for the client to invoke, and `auto_load_toolsets`
    /// cannot help because it only fires once a call is actually attempted.
    ///
    /// `None` means "not stated in config". Unlike `dispatcher_tools` this has
    /// no client-specific default -- it is off everywhere unless asked for,
    /// because 208 tool schemas is a cost no client should pay by surprise.
    #[serde(default)]
    pub eager_toolsets: Option<bool>,

    /// Expose the three dispatcher tools (`list_available_tools`,
    /// `get_tool_schema`, `execute_konnect_tool`).
    ///
    /// This is the cheap answer to a client that caches its first `tools/list`
    /// and never refreshes on `notifications/tools/list_changed`. For such a
    /// client the `load_toolset` discovery loop cannot work -- the tools it
    /// activates never reach the client as callable schemas (#134, #169) -- but
    /// three always-present tools can reach all 208 on demand, for roughly a
    /// tenth of the context that pre-loading them costs.
    ///
    /// `None` means "not stated", which resolves to `true`: on by default for
    /// every client. See [`Config::dispatcher_tools_for`] for why.
    #[serde(default)]
    pub dispatcher_tools: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TransportMode {
    #[default]
    Stdio,
    Http,
    Both,
}

fn default_kicad_cli() -> String {
    if cfg!(target_os = "windows") {
        "kicad-cli.exe".to_string()
    } else {
        "kicad-cli".to_string()
    }
}

fn default_kicad_binary() -> String {
    if cfg!(target_os = "windows") {
        "kicad.exe".to_string()
    } else {
        "kicad".to_string()
    }
}

fn default_ipc_address() -> String {
    // Empty = auto-detect from KICAD_API_SOCKET env var at runtime
    std::env::var("KICAD_API_SOCKET").unwrap_or_default()
}

fn default_http_address() -> String {
    "127.0.0.1:3000".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Config {
    /// Resolve a three-way setting: CLI switch beats config, config beats the
    /// launching client's default.
    ///
    /// Keeping "unset" distinct from "explicitly off" is the whole reason the
    /// config fields are `Option<bool>`. Collapse them and a client default can
    /// no longer be expressed, which is how Codex ended up on the lazy path it
    /// cannot use.
    fn resolve(cli: Option<bool>, config: Option<bool>, client_default: bool) -> bool {
        cli.or(config).unwrap_or(client_default)
    }

    /// Whether every toolset is pre-loaded at startup.
    ///
    /// No client turns this on by default. It is the heavyweight option --
    /// roughly 34K tokens of tool schemas in every request for the whole task --
    /// and `dispatcher_tools` reaches the same tools for a fraction of it. Kept
    /// because a caller who wants native schemas for everything, and has the
    /// context budget, should be able to say so.
    pub fn eager_toolsets_for(&self, cli_override: Option<bool>) -> bool {
        Self::resolve(cli_override, self.eager_toolsets, false)
    }

    /// Whether the three dispatcher tools are exposed. On unless turned off.
    ///
    /// This was briefly keyed to the launching client, on the theory that only
    /// Codex caches its first `tools/list` and Claude Code refreshes on
    /// `notifications/tools/list_changed`. That theory does not survive
    /// contact with how the server is actually launched: #134 and #169 were
    /// reported against Claude *Desktop*, which shares `InstallClient::Claude`
    /// with Claude Code and passes no `--client` at all (see
    /// `examples/claude_desktop_config.example.json`). The client that the bug
    /// was reported against was the one arrangement left without the fix.
    ///
    /// There is no reliable way to tell those two apart at startup, so the
    /// dispatcher is on for everyone. It costs ~628 tokens against the ~2.2K
    /// baseline — cheap enough that paying it on a client which does not need
    /// it beats leaving a client that does need it broken by default.
    /// `--no-dispatcher-tools` opts out.
    pub fn dispatcher_tools_for(&self, cli_override: Option<bool>) -> bool {
        Self::resolve(cli_override, self.dispatcher_tools, true)
    }

    /// Load config from the default search path.
    pub fn load() -> Result<Self> {
        let mut config_paths = vec![
            PathBuf::from("konnect.toml"),
            PathBuf::from("settings.json"),
        ];
        config_paths.extend(exe_relative_settings_paths());
        config_paths.push(dirs_config_path());

        let mut config = None;
        for path in &config_paths {
            if path.exists() {
                config = Some(Self::load_from(path)?);
                break;
            }
        }

        let mut config = config.unwrap_or_default();
        config.apply_env_fallbacks();
        Ok(config)
    }

    /// Env var wins over an unset/blank ipc_address either way. Must run on
    /// every load path — including `--config <file>`, which is how KiCAD
    /// itself launches the server (with KICAD_API_SOCKET in the environment).
    pub fn apply_env_fallbacks(&mut self) {
        if self.ipc_address.is_empty() {
            if let Ok(sock) = std::env::var("KICAD_API_SOCKET") {
                if !sock.is_empty() {
                    self.ipc_address = sock;
                }
            }
        }
    }

    /// Load config from a specific file path. Auto-detects JSON vs TOML by extension.
    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

        match ext {
            "json" => {
                let config: Config = serde_json::from_str(&content)?;
                Ok(config)
            }
            _ => {
                // Default: TOML
                let config: Config = toml::from_str(&content)?;
                Ok(config)
            }
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            kicad_cli: default_kicad_cli(),
            kicad_binary: default_kicad_binary(),
            project_dir: None,
            ipc_address: default_ipc_address(),
            transport: TransportMode::default(),
            http_address: default_http_address(),
            jlcpcb_db_path: None,
            log_level: default_log_level(),
            auto_load_toolsets: false,
            eager_toolsets: None,
            dispatcher_tools: None,
        }
    }
}

/// settings.json next to the binary, and one dir up (covers <plugin_dir>/bin/konnect).
fn exe_relative_settings_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            paths.push(exe_dir.join("settings.json"));
            if let Some(parent_dir) = exe_dir.parent() {
                paths.push(parent_dir.join("settings.json"));
            }
        }
    }
    paths
}

fn dirs_config_path() -> PathBuf {
    // Platform-specific config directory
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        PathBuf::from(appdata).join("konnect").join("config.toml")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("konnect")
            .join("config.toml")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home)
            .join(".config")
            .join("konnect")
            .join("config.toml")
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(ext: &str, content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(&format!(".{ext}"))
            .tempfile()
            .unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    // Malformed input must produce Err, never a panic (the class of bug
    // PR #9 found in the config *tools*; this pins the server config too).

    #[test]
    fn json_non_object_root_is_err_not_panic() {
        for bad in ["[1, 2, 3]", "42", "\"just a string\"", "null", "true"] {
            let f = write_temp("json", bad);
            assert!(Config::load_from(f.path()).is_err(), "input: {bad}");
        }
    }

    #[test]
    fn json_wrong_field_types_are_err() {
        for bad in [
            r#"{"transport": 42}"#,
            r#"{"transport": "carrier-pigeon"}"#,
            r#"{"kicad_cli": ["a", "b"]}"#,
            r#"{"log_level": {"nested": true}}"#,
        ] {
            let f = write_temp("json", bad);
            assert!(Config::load_from(f.path()).is_err(), "input: {bad}");
        }
    }

    #[test]
    fn toml_garbage_is_err_not_panic() {
        for bad in ["= = =", "[unclosed", "transport = ", "\u{0000}\u{FFFF}"] {
            let f = write_temp("toml", bad);
            assert!(Config::load_from(f.path()).is_err(), "input: {bad:?}");
        }
    }

    #[test]
    fn missing_file_is_err() {
        assert!(Config::load_from(std::path::Path::new("does/not/exist.toml")).is_err());
    }

    // Partial configs fill in defaults for everything omitted.

    #[test]
    fn empty_json_object_yields_defaults() {
        let f = write_temp("json", "{}");
        let c = Config::load_from(f.path()).unwrap();
        let d = Config::default();
        assert_eq!(c.kicad_cli, d.kicad_cli);
        assert_eq!(c.http_address, d.http_address);
        assert_eq!(c.log_level, d.log_level);
        assert!(matches!(c.transport, TransportMode::Stdio));
    }

    #[test]
    fn empty_toml_yields_defaults() {
        let f = write_temp("toml", "");
        let c = Config::load_from(f.path()).unwrap();
        assert_eq!(c.log_level, "info");
    }

    #[test]
    fn partial_toml_overrides_only_named_fields() {
        let f = write_temp(
            "toml",
            "transport = \"http\"\nhttp_address = \"127.0.0.1:9999\"\n",
        );
        let c = Config::load_from(f.path()).unwrap();
        assert!(matches!(
            c.transport,
            TransportMode::Both | TransportMode::Http
        ));
        assert!(matches!(c.transport, TransportMode::Http));
        assert_eq!(c.http_address, "127.0.0.1:9999");
        assert_eq!(c.log_level, "info"); // untouched default
    }

    // Mutates the process-wide env var, so these two run serially.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn empty_ipc_address_falls_back_to_env_var_when_no_config_found() {
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::set_var("KICAD_API_SOCKET", "ipc://env-fallback.sock");
        let c = Config::default();
        assert_eq!(c.ipc_address, "ipc://env-fallback.sock");
        std::env::remove_var("KICAD_API_SOCKET");
    }

    #[test]
    fn explicit_empty_ipc_address_in_config_file_does_not_block_env_var() {
        // A present-but-blank field must not out-rank the env var the way
        // a merely-missing field would (#39).
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::set_var("KICAD_API_SOCKET", "ipc://env-wins.sock");

        let f = write_temp("json", r#"{"ipc_socket_path": ""}"#);
        let mut c = Config::load_from(f.path()).unwrap();
        assert_eq!(c.ipc_address, "", "sanity: file's blank value loaded as-is");

        c.apply_env_fallbacks();
        assert_eq!(c.ipc_address, "ipc://env-wins.sock");

        // But an explicit file value must out-rank the env var.
        let f = write_temp("json", r#"{"ipc_socket_path": "ipc://file-wins.sock"}"#);
        let mut c = Config::load_from(f.path()).unwrap();
        c.apply_env_fallbacks();
        assert_eq!(c.ipc_address, "ipc://file-wins.sock");

        std::env::remove_var("KICAD_API_SOCKET");
    }

    #[test]
    fn legacy_ipc_socket_path_alias_still_works() {
        // settings.json written by the KiCAD plugin dialog uses the alias.
        let f = write_temp("json", r#"{"ipc_socket_path": "ipc://test.sock"}"#);
        let c = Config::load_from(f.path()).unwrap();
        assert_eq!(c.ipc_address, "ipc://test.sock");
    }

    #[test]
    fn unknown_extension_parses_as_toml() {
        let f = write_temp("conf", "log_level = \"debug\"\n");
        let c = Config::load_from(f.path()).unwrap();
        assert_eq!(c.log_level, "debug");
    }

    // ─── exposure resolution ────────────────────────────────────────────────
    //
    // The whole point of the Option is telling "the user asked for off" apart
    // from "the user said nothing". Collapsing those was the bug: a Codex user
    // who never wrote a config got the starter kit and could not call any tool
    // load_toolset claimed to have loaded (#134, #169).

    /// Unstated means on. Claude Desktop is the client #134/#169 were reported
    /// against and it passes no `--client`, so anything that leaves the default
    /// off leaves the original bug in place for the reporter.
    #[test]
    fn dispatcher_tools_default_on_when_unstated() {
        let c = Config::default();
        assert!(c.dispatcher_tools.is_none(), "default must stay unstated");
        assert!(c.dispatcher_tools_for(None), "unstated must resolve to on");
    }

    /// Eager loading has no client default: it costs roughly ten times the
    /// dispatcher for the same reach, so no client should get it by surprise.
    #[test]
    fn eager_toolsets_is_off_until_asked_for() {
        let c = Config::default();
        assert!(c.eager_toolsets.is_none());
        assert!(!c.eager_toolsets_for(None));
        assert!(c.eager_toolsets_for(Some(true)), "CLI can still turn it on");
    }

    /// A value written in config beats the client default in both directions --
    /// including turning the dispatcher off for Codex.
    #[test]
    fn config_overrides_the_client_default() {
        let off = Config {
            dispatcher_tools: Some(false),
            ..Config::default()
        };
        assert!(
            !off.dispatcher_tools_for(None),
            "config must be able to turn it off"
        );

        let eager = Config {
            eager_toolsets: Some(true),
            ..Config::default()
        };
        assert!(eager.eager_toolsets_for(None));
    }

    /// And the CLI switch beats config, so a user can flip one launch without
    /// editing a file that other launches share.
    #[test]
    fn cli_override_beats_config_and_client() {
        let off = Config {
            dispatcher_tools: Some(false),
            ..Config::default()
        };
        assert!(off.dispatcher_tools_for(Some(true)));

        let on = Config {
            dispatcher_tools: Some(true),
            ..Config::default()
        };
        assert!(!on.dispatcher_tools_for(Some(false)));

        let eager = Config {
            eager_toolsets: Some(true),
            ..Config::default()
        };
        assert!(!eager.eager_toolsets_for(Some(false)));
    }

    #[test]
    fn eager_toolsets_still_parses_from_a_config_file() {
        let f = write_temp("toml", "eager_toolsets = true\n");
        assert_eq!(
            Config::load_from(f.path()).unwrap().eager_toolsets,
            Some(true)
        );

        let f = write_temp("toml", "eager_toolsets = false\n");
        assert_eq!(
            Config::load_from(f.path()).unwrap().eager_toolsets,
            Some(false)
        );

        let f = write_temp("toml", "log_level = \"debug\"\n");
        assert_eq!(Config::load_from(f.path()).unwrap().eager_toolsets, None);
        assert_eq!(Config::load_from(f.path()).unwrap().dispatcher_tools, None);

        let f = write_temp("toml", "dispatcher_tools = true\n");
        assert_eq!(
            Config::load_from(f.path()).unwrap().dispatcher_tools,
            Some(true)
        );
    }
}
