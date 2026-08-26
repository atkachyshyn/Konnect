//! Client-aware installer for Konnect's bundled guidance.
//!
//! Handles client-scoped install, uninstall, status, first-launch setup, and
//! Claude hook integration without writing into another client's directories.

use crate::manifest::{AGENTS, HOOK_SKILLS, SKILLS};
use anyhow::{bail, Context, Result};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InstallClient {
    #[default]
    Claude,
    Codex,
}

impl InstallClient {
    /// Whether this client caches the tool list from the first `tools/list` and
    /// never re-fetches it on `notifications/tools/list_changed`.
    ///
    /// For such a client the router's lazy loading is not a context saving but
    /// a wall: `load_toolset` activates the tools server-side and reports their
    /// names, yet the client never receives their schemas, so it has nothing to
    /// invoke and every tool outside the starter kit stays uncallable for the
    /// whole session (#134, #169).
    ///
    /// These clients get the dispatcher tools by default, which reach the whole
    /// catalogue without enlarging `tools/list`. Pre-loading every toolset
    /// (`--eager-toolsets`) also cures it but costs about ten times the context,
    /// so it is opt-in rather than the default.
    pub fn caches_initial_tool_list(self) -> bool {
        match self {
            // Codex reads tools/list once per task and ignores the change
            // notification.
            Self::Codex => true,
            // Claude Code re-fetches on notifications/tools/list_changed, so it
            // keeps the small ~2K baseline and expands on demand.
            Self::Claude => false,
        }
    }

    fn marker_name(self) -> &'static str {
        match self {
            Self::Claude => ".installed-claude",
            Self::Codex => ".installed-codex",
        }
    }
}

impl fmt::Display for InstallClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Claude => write!(f, "Claude"),
            Self::Codex => write!(f, "Codex"),
        }
    }
}

impl FromStr for InstallClient {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            _ => bail!("unsupported client '{value}'; expected 'claude' or 'codex'"),
        }
    }
}

/// Parse `--client <claude|codex>` from a subcommand's arguments, refusing any
/// argument this build does not recognise. Claude remains the default for
/// compatibility.
///
/// Skipping unrecognised arguments is how `konnect init --help` came to run the
/// installer (#238): `--help` was not `--client`, so it was dropped on the
/// floor, the client defaulted to Claude, and a command that looks like
/// documentation rewrote `~/.claude`. A misspelled flag has the same shape —
/// it changes nothing the caller asked for and everything they did not.
pub fn client_from_args(args: &[String]) -> Result<InstallClient> {
    client_from_args_allowing(args, &[], &[])
}

/// Valueless flags the server invocation accepts alongside `--client`.
const SERVER_FLAGS: &[&str] = &[
    "--eager-toolsets",
    "--no-eager-toolsets",
    "--dispatcher-tools",
    "--no-dispatcher-tools",
];

/// As [`client_from_args`], for the server invocation — which also carries
/// `--config <path>`, parsed elsewhere in `main`, and the eager-toolset
/// switches parsed by [`eager_toolsets_from_server_args`].
pub fn client_from_server_args(args: &[String]) -> Result<InstallClient> {
    client_from_args_allowing(args, &["--config"], SERVER_FLAGS)
}

/// Explicit `--eager-toolsets` / `--no-eager-toolsets` override, or `None` when
/// neither was given so config and the client default decide.
///
/// This is the escape hatch in both directions: a Codex user on a tight context
/// budget can force the small starter listing, and a Claude user whose client
/// misses `notifications/tools/list_changed` can force the full one — without
/// editing `konnect.toml`.
pub fn eager_toolsets_from_server_args(args: &[String]) -> Result<Option<bool>> {
    bool_flag(args, "eager-toolsets")
}

/// Explicit `--dispatcher-tools` / `--no-dispatcher-tools` override, or `None`
/// when neither was given so config and the client default decide.
pub fn dispatcher_tools_from_server_args(args: &[String]) -> Result<Option<bool>> {
    bool_flag(args, "dispatcher-tools")
}

/// Read a `--<name>` / `--no-<name>` pair into an `Option<bool>`.
///
/// Repeating the same flag is harmless; asking for both directions is a
/// contradiction, and honouring whichever came last would hand back the
/// opposite of what half the command line asked for.
fn bool_flag(args: &[String], name: &str) -> Result<Option<bool>> {
    let on = format!("--{name}");
    let off = format!("--no-{name}");
    let mut selected = None;
    for arg in args {
        let value = if *arg == on {
            true
        } else if *arg == off {
            false
        } else {
            continue;
        };
        if selected.is_some_and(|prev| prev != value) {
            bail!("--{name} and --no-{name} are mutually exclusive");
        }
        selected = Some(value);
    }
    Ok(selected)
}

/// `also` names options that take a value and that this parser should step
/// over rather than reject; `flags` names valueless options to skip.
fn client_from_args_allowing(
    args: &[String],
    also: &[&str],
    flags: &[&str],
) -> Result<InstallClient> {
    let mut selected = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--client" {
            if selected.is_some() {
                bail!("--client may only be specified once");
            }
            let value = args
                .get(index + 1)
                .context("--client requires 'claude' or 'codex'")?;
            selected = Some(value.parse()?);
            index += 2;
        } else if also.contains(&arg) {
            index += 2;
        } else if flags.contains(&arg) {
            index += 1;
        } else {
            bail!("unrecognised argument '{arg}'; run 'konnect --help' for usage");
        }
    }
    Ok(selected.unwrap_or_default())
}

pub fn run_install(client: InstallClient) -> Result<()> {
    run_install_at(client, &InstallPaths::for_current_user()?, true)
}

pub fn run_install_silent(client: InstallClient) -> Result<()> {
    run_install_at(client, &InstallPaths::for_current_user()?, false)
}

pub fn run_uninstall(client: InstallClient) -> Result<()> {
    run_uninstall_at(client, &InstallPaths::for_current_user()?, true)
}

pub fn print_status(client: InstallClient) -> Result<()> {
    print_status_at(client, &InstallPaths::for_current_user()?)
}

/// Print bundled guidance for Claude hook integration.
pub fn print_skill_content(name: &str) -> Result<()> {
    for hook in HOOK_SKILLS {
        if hook.name == name {
            print!("{}", hook.content);
            return Ok(());
        }
    }
    for skill in SKILLS {
        if skill.name == name {
            print!("{}", skill.content);
            return Ok(());
        }
    }
    eprintln!("Unknown skill: {}", name);
    std::process::exit(1);
}

pub fn needs_install(client: InstallClient) -> bool {
    InstallPaths::for_current_user()
        .map(|paths| !has_install_marker(client, &paths))
        .unwrap_or(false)
}

/// Double-click behavior remains Claude-focused for backward compatibility.
pub fn run_double_click_install() -> Result<()> {
    println!("===========================================");
    println!("  Konnect v{}", env!("CARGO_PKG_VERSION"));
    println!("  First-time Setup");
    println!("===========================================\n");
    run_install(InstallClient::Claude)?;

    let exe = std::env::current_exe()?;
    let exe_str = exe.to_string_lossy().replace('\\', "\\\\");
    println!("\n-------------------------------------------");
    println!("Add this to your Claude MCP config:");
    println!("-------------------------------------------\n");
    println!(r#"  "konnect": {{"#);
    println!(r#"    "command": "{}","#, exe_str);
    println!(r#"    "env": {{ "RUST_LOG": "info" }}"#);
    println!(r#"  }}"#);
    println!("\nConfig locations:");
    println!("  Claude Desktop: %APPDATA%\\Claude\\claude_desktop_config.json");
    println!("  Claude Code:    .mcp.json in your project root");
    println!("\nAfter editing the config, restart Claude.\n");
    println!("Press Enter to close...");
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
    Ok(())
}

#[derive(Debug)]
struct InstallPaths {
    home: PathBuf,
}

impl InstallPaths {
    fn for_current_user() -> Result<Self> {
        Ok(Self {
            home: dirs::home_dir().context("could not locate home directory")?,
        })
    }

    fn data_dir(&self) -> PathBuf {
        self.home.join(".konnect")
    }

    fn skills_dir(&self, client: InstallClient) -> PathBuf {
        match client {
            InstallClient::Claude => self.home.join(".claude").join("skills"),
            InstallClient::Codex => self.home.join(".agents").join("skills"),
        }
    }

    fn claude_agents_dir(&self) -> PathBuf {
        self.home.join(".claude").join("agents")
    }

    fn claude_settings_path(&self) -> PathBuf {
        self.home.join(".claude").join("settings.json")
    }

    fn marker(&self, client: InstallClient) -> PathBuf {
        self.data_dir().join(client.marker_name())
    }

    fn legacy_marker(&self) -> PathBuf {
        self.data_dir().join(".installed")
    }
}

fn run_install_at(client: InstallClient, paths: &InstallPaths, verbose: bool) -> Result<()> {
    if verbose {
        match client {
            InstallClient::Claude => {
                println!("Installing Konnect skills, agents, and hooks for Claude...\n")
            }
            InstallClient::Codex => println!("Installing Konnect skills for Codex...\n"),
        }
    }

    let skill_count = install_skills(client, paths, verbose)?;
    let mut agent_count = 0;
    let mut hook_count = 0;
    if client == InstallClient::Claude {
        let agents_dir = paths.claude_agents_dir();
        fs::create_dir_all(&agents_dir)?;
        for agent in AGENTS {
            fs::write(agents_dir.join(agent.filename), agent.content)?;
            agent_count += 1;
            if verbose {
                println!("  [+] Agent: {}", agent.filename);
            }
        }

        let exe = std::env::current_exe()?;
        let settings_path = paths.claude_settings_path();
        let exe_str = exe.to_string_lossy();
        hook_count = if verbose {
            patch_claude_settings(&settings_path, &exe_str)?
        } else {
            patch_claude_settings(&settings_path, &exe_str).unwrap_or_default()
        };
        if verbose {
            if hook_count > 0 {
                println!("  [+] Hooks: {hook_count} entries patched into settings.json");
            } else {
                println!("  [=] Hooks: already installed (no changes)");
            }
        }
    }

    if verbose {
        if let Some(kicad_path) = detect_kicad() {
            println!("\n  [+] Found KiCAD at: {}", kicad_path.display());
        } else {
            println!("\n  [-] KiCAD not found in standard locations");
            println!("      Set kicad_cli path in your config file manually");
        }
    }

    fs::create_dir_all(paths.data_dir())?;
    fs::write(paths.marker(client), env!("CARGO_PKG_VERSION"))?;

    if verbose {
        match client {
            InstallClient::Claude => println!(
                "\nDone: {skill_count} skills, {agent_count} agents, {hook_count} hooks installed for Claude."
            ),
            InstallClient::Codex => {
                println!("\nDone: {skill_count} skills installed for Codex.")
            }
        }
    } else {
        eprintln!(
            "[konnect] Silent {client} install complete: {skill_count} skills, {agent_count} agents"
        );
    }
    Ok(())
}

fn install_skills(client: InstallClient, paths: &InstallPaths, verbose: bool) -> Result<usize> {
    let skills_dir = paths.skills_dir(client);
    for skill in SKILLS {
        let dest = skills_dir.join(skill.name);
        fs::create_dir_all(&dest)?;
        fs::write(dest.join("SKILL.md"), skill.content)?;
        if !skill.references.is_empty() {
            let refs_dir = dest.join("references");
            fs::create_dir_all(&refs_dir)?;
            for (filename, content) in skill.references {
                fs::write(refs_dir.join(filename), content)?;
            }
        }
        if verbose {
            println!("  [+] Skill: {}", skill.name);
        }
    }
    Ok(SKILLS.len())
}

fn run_uninstall_at(client: InstallClient, paths: &InstallPaths, verbose: bool) -> Result<()> {
    if verbose {
        println!("Uninstalling Konnect guidance for {client}...\n");
    }

    let skills_dir = paths.skills_dir(client);
    for skill in SKILLS {
        let dest = skills_dir.join(skill.name);
        if dest.exists() {
            fs::remove_dir_all(&dest)?;
            if verbose {
                println!("  [-] Removed skill: {}", skill.name);
            }
        }
    }

    if client == InstallClient::Claude {
        let agents_dir = paths.claude_agents_dir();
        for agent in AGENTS {
            let dest = agents_dir.join(agent.filename);
            if dest.exists() {
                fs::remove_file(&dest)?;
                if verbose {
                    println!("  [-] Removed agent: {}", agent.filename);
                }
            }
        }
        remove_hooks_from_settings(&paths.claude_settings_path())?;
        if verbose {
            println!("  [-] Removed hook entries from settings.json");
        }
        remove_if_present(&paths.legacy_marker())?;
    }

    remove_if_present(&paths.marker(client))?;
    if verbose {
        println!("\nDone.");
    }
    Ok(())
}

fn print_status_at(client: InstallClient, paths: &InstallPaths) -> Result<()> {
    println!(
        "Konnect v{} — {client} Install Status\n",
        env!("CARGO_PKG_VERSION")
    );
    let skills_dir = paths.skills_dir(client);
    println!("Skills ({}):", display_home_path(&skills_dir, &paths.home));
    for skill in SKILLS {
        let marker = if skills_dir.join(skill.name).join("SKILL.md").exists() {
            "+"
        } else {
            "-"
        };
        println!("  [{marker}] {}", skill.name);
    }

    if client == InstallClient::Claude {
        let agents_dir = paths.claude_agents_dir();
        println!("\nAgents (~/.claude/agents/):");
        for agent in AGENTS {
            let marker = if agents_dir.join(agent.filename).exists() {
                "+"
            } else {
                "-"
            };
            println!("  [{marker}] {}", agent.filename);
        }
        println!("\nHooks (~/.claude/settings.json):");
        let raw = fs::read_to_string(paths.claude_settings_path()).unwrap_or_default();
        for hook in HOOK_SKILLS {
            let marker = if raw.contains(hook.name) { "+" } else { "-" };
            println!("  [{marker}] {} ({})", hook.name, hook.event);
        }
    }

    println!("\nKiCAD:");
    if let Some(path) = detect_kicad() {
        println!("  [+] Found: {}", path.display());
    } else {
        println!("  [-] Not found in standard locations");
    }

    if let Some(marker) = install_marker(client, paths) {
        let version = fs::read_to_string(marker).unwrap_or_default();
        println!("\nInstall marker: v{}", version.trim());
    } else {
        println!("\nInstall marker: not present (never installed)");
    }
    Ok(())
}

fn install_marker(client: InstallClient, paths: &InstallPaths) -> Option<PathBuf> {
    let marker = paths.marker(client);
    if marker.exists() {
        return Some(marker);
    }
    if client == InstallClient::Claude && paths.legacy_marker().exists() {
        return Some(paths.legacy_marker());
    }
    None
}

fn has_install_marker(client: InstallClient, paths: &InstallPaths) -> bool {
    install_marker(client, paths).is_some()
}

fn remove_if_present(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn display_home_path(path: &Path, home: &Path) -> String {
    path.strip_prefix(home)
        .map(|relative| format!("~/{}", relative.display()))
        .unwrap_or_else(|_| path.display().to_string())
}

fn patch_claude_settings(path: &Path, exe_str: &str) -> Result<usize> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = if path.exists() {
        fs::read_to_string(path)?
    } else {
        "{}".to_string()
    };
    let mut settings: serde_json::Value = serde_json::from_str(&raw)?;
    let hooks_obj = settings
        .as_object_mut()
        .context("Claude settings root is not an object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("hooks field is not an object")?;

    let mut added = 0;
    for hook in HOOK_SKILLS {
        let event_arr = hooks_obj
            .entry(hook.event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .context("hook event field is not an array")?;
        let already_exists = event_arr.iter().any(|entry| {
            entry
                .get("hooks")
                .and_then(|hooks| hooks.as_array())
                .is_some_and(|hooks| {
                    hooks.iter().any(|hook_entry| {
                        hook_entry
                            .get("command")
                            .and_then(|command| command.as_str())
                            .is_some_and(|command| {
                                command.contains("konnect") && command.contains(hook.name)
                            })
                    })
                })
        });
        if !already_exists {
            // Preserve the established Claude hook command representation.
            let exe_escaped = exe_str.replace('\\', "\\\\");
            event_arr.push(serde_json::json!({
                "matcher": hook.tool_matcher,
                "hooks": [{
                    "type": "command",
                    "command": format!("{} skill {}", exe_escaped, hook.name)
                }]
            }));
            added += 1;
        }
    }
    fs::write(path, serde_json::to_string_pretty(&settings)?)?;
    Ok(added)
}

fn remove_hooks_from_settings(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(path)?;
    let mut settings: serde_json::Value = serde_json::from_str(&raw)?;
    if let Some(hooks_obj) = settings
        .get_mut("hooks")
        .and_then(|hooks| hooks.as_object_mut())
    {
        for hook in HOOK_SKILLS {
            if let Some(event_arr) = hooks_obj
                .get_mut(hook.event)
                .and_then(|event| event.as_array_mut())
            {
                event_arr.retain(|entry| {
                    !entry
                        .get("hooks")
                        .and_then(|hooks| hooks.as_array())
                        .is_some_and(|hooks| {
                            hooks.iter().any(|hook_entry| {
                                hook_entry
                                    .get("command")
                                    .and_then(|command| command.as_str())
                                    .is_some_and(|command| command.contains("konnect"))
                            })
                        })
                });
            }
        }
    }
    fs::write(path, serde_json::to_string_pretty(&settings)?)?;
    Ok(())
}

/// Auto-detect a KiCad installation.
pub fn detect_kicad() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let standard_paths: Vec<PathBuf> = [
        r"C:\KiCad\10.0\bin\kicad-cli.exe",
        r"C:\Program Files\KiCad\10.0\bin\kicad-cli.exe",
        r"C:\Program Files (x86)\KiCad\10.0\bin\kicad-cli.exe",
        r"C:\KiCad\9.0\bin\kicad-cli.exe",
        r"C:\Program Files\KiCad\9.0\bin\kicad-cli.exe",
        r"C:\Program Files (x86)\KiCad\9.0\bin\kicad-cli.exe",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();

    #[cfg(target_os = "macos")]
    let standard_paths: Vec<PathBuf> = {
        let mut paths = vec![
            PathBuf::from("/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli"),
            PathBuf::from("/usr/local/bin/kicad-cli"),
        ];
        if let Ok(home) = std::env::var("HOME") {
            paths.push(
                PathBuf::from(home).join("Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli"),
            );
        }
        paths
    };

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let standard_paths: Vec<PathBuf> = vec![
        PathBuf::from("/usr/bin/kicad-cli"),
        PathBuf::from("/usr/local/bin/kicad-cli"),
    ];

    for path in &standard_paths {
        if path.exists() {
            return Some(path.clone());
        }
    }
    #[cfg(target_os = "windows")]
    if let Some(path) = detect_kicad_from_registry() {
        return Some(path);
    }
    None
}

#[cfg(target_os = "windows")]
fn detect_kicad_from_registry() -> Option<PathBuf> {
    use std::process::Command;
    let output = Command::new("reg")
        .args(["query", r"HKLM\SOFTWARE\KiCad\10.0", "/ve"])
        .output()
        .ok()?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains("REG_SZ") {
                let path_str = line.split("REG_SZ").last()?.trim();
                let cli_path = Path::new(path_str).join("bin").join("kicad-cli.exe");
                if cli_path.exists() {
                    return Some(cli_path);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_paths(temp: &TempDir) -> InstallPaths {
        InstallPaths {
            home: temp.path().to_path_buf(),
        }
    }

    #[test]
    fn client_argument_defaults_to_claude() {
        assert_eq!(client_from_args(&[]).unwrap(), InstallClient::Claude);
    }

    /// An argument this build does not understand stops the command instead of
    /// being skipped on the way to a default.
    ///
    /// The skipping is what made `konnect init --help` install (#238), and the
    /// same hole means a typo — `--cleint codex` — silently installs for
    /// Claude, which is the surprising outcome for someone mid-way through
    /// setting up Codex.
    #[test]
    fn an_unrecognised_argument_stops_the_command() {
        for argv in [
            vec!["--help".to_string()],
            vec!["-h".to_string()],
            vec!["--cleint".to_string(), "codex".to_string()],
            vec![
                "--client".to_string(),
                "codex".to_string(),
                "extra".to_string(),
            ],
        ] {
            let error = client_from_args(&argv)
                .expect_err(&format!("{argv:?} must not resolve to a client"));
            let message = format!("{error:#}");
            assert!(
                message.contains("unrecognised argument") || message.contains("unsupported client"),
                "{argv:?}: {message}"
            );
        }
    }

    /// The server invocation carries `--config <path>`, which the bundled
    /// `examples/*.json` tell users to write, so it must still parse.
    #[test]
    fn server_arguments_accept_config_and_still_reject_the_unknown() {
        let with_config = vec![
            "--config".to_string(),
            "C:/konnect.json".to_string(),
            "--client".to_string(),
            "codex".to_string(),
        ];
        assert_eq!(
            client_from_server_args(&with_config).unwrap(),
            InstallClient::Codex
        );
        assert_eq!(
            client_from_server_args(&["--config".to_string(), "C:/k.json".to_string()]).unwrap(),
            InstallClient::Claude
        );
        assert!(client_from_server_args(&["--nope".to_string()]).is_err());
    }

    /// Codex is the client the router's lazy loading cannot serve, and Claude
    /// is the one it can. If this ever flips silently, Codex users get the
    /// starter kit and a set of tools that report as loaded but cannot be
    /// called (#134, #169).
    #[test]
    fn only_codex_caches_the_initial_tool_list() {
        assert!(InstallClient::Codex.caches_initial_tool_list());
        assert!(!InstallClient::Claude.caches_initial_tool_list());
    }

    #[test]
    fn eager_toolset_flags_parse_in_both_directions() {
        assert_eq!(eager_toolsets_from_server_args(&[]).unwrap(), None);
        assert_eq!(
            eager_toolsets_from_server_args(&["--eager-toolsets".to_string()]).unwrap(),
            Some(true)
        );
        assert_eq!(
            eager_toolsets_from_server_args(&["--no-eager-toolsets".to_string()]).unwrap(),
            Some(false)
        );
    }

    /// Asking for both is a contradiction, and silently honouring the last one
    /// would hand back the opposite of what half the command line asked for.
    #[test]
    fn contradictory_eager_toolset_flags_are_rejected() {
        let both = vec![
            "--eager-toolsets".to_string(),
            "--no-eager-toolsets".to_string(),
        ];
        assert!(eager_toolsets_from_server_args(&both).is_err());

        // Repeating the same flag is harmless, not a contradiction.
        let twice = vec![
            "--eager-toolsets".to_string(),
            "--eager-toolsets".to_string(),
        ];
        assert_eq!(eager_toolsets_from_server_args(&twice).unwrap(), Some(true));
    }

    /// The server arg parser rejects anything it does not know, so the new
    /// flags have to be declared there too — otherwise adding them to a client
    /// config turns every launch into a startup error.
    #[test]
    fn server_args_accept_the_eager_toolset_flags_alongside_client() {
        let args = vec![
            "--client".to_string(),
            "codex".to_string(),
            "--no-eager-toolsets".to_string(),
            "--config".to_string(),
            "C:/konnect.json".to_string(),
        ];
        assert_eq!(
            client_from_server_args(&args).unwrap(),
            InstallClient::Codex
        );
        assert_eq!(eager_toolsets_from_server_args(&args).unwrap(), Some(false));

        // And they remain rejected where they are not valid — `init` and
        // friends take no such flag.
        assert!(client_from_args(&["--eager-toolsets".to_string()]).is_err());
    }

    #[test]
    fn client_argument_selects_codex() {
        let args = vec!["--client".into(), "codex".into()];
        assert_eq!(client_from_args(&args).unwrap(), InstallClient::Codex);
    }

    #[test]
    fn client_argument_rejects_bad_values() {
        assert!(client_from_args(&["--client".into()]).is_err());
        assert!(client_from_args(&["--client".into(), "other".into()]).is_err());
        assert!(client_from_args(&[
            "--client".into(),
            "codex".into(),
            "--client".into(),
            "claude".into(),
        ])
        .is_err());
    }

    #[test]
    fn codex_install_writes_skills_without_touching_claude() {
        let temp = TempDir::new().unwrap();
        let paths = test_paths(&temp);
        run_install_at(InstallClient::Codex, &paths, false).unwrap();

        for skill in SKILLS {
            let skill_dir = paths.skills_dir(InstallClient::Codex).join(skill.name);
            assert_eq!(
                fs::read_to_string(skill_dir.join("SKILL.md")).unwrap(),
                skill.content
            );
            for (filename, content) in skill.references {
                assert_eq!(
                    fs::read_to_string(skill_dir.join("references").join(filename)).unwrap(),
                    *content
                );
            }
        }
        assert!(!temp.path().join(".claude").exists());
        assert!(paths.marker(InstallClient::Codex).exists());
        assert!(!paths.marker(InstallClient::Claude).exists());
    }

    #[test]
    fn claude_install_is_idempotent_and_preserves_settings() {
        let temp = TempDir::new().unwrap();
        let paths = test_paths(&temp);
        let settings_path = paths.claude_settings_path();
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(&settings_path, r#"{"theme":"dark"}"#).unwrap();

        run_install_at(InstallClient::Claude, &paths, false).unwrap();
        run_install_at(InstallClient::Claude, &paths, false).unwrap();

        for skill in SKILLS {
            assert!(paths
                .skills_dir(InstallClient::Claude)
                .join(skill.name)
                .join("SKILL.md")
                .exists());
        }
        for agent in AGENTS {
            assert!(paths.claude_agents_dir().join(agent.filename).exists());
        }
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(settings_path).unwrap()).unwrap();
        assert_eq!(settings["theme"], "dark");
        for hook in HOOK_SKILLS {
            assert_eq!(settings["hooks"][hook.event].as_array().unwrap().len(), 1);
        }
    }

    #[test]
    fn legacy_marker_applies_to_claude_only() {
        let temp = TempDir::new().unwrap();
        let paths = test_paths(&temp);
        fs::create_dir_all(paths.data_dir()).unwrap();
        fs::write(paths.legacy_marker(), "0.4.0").unwrap();
        assert!(has_install_marker(InstallClient::Claude, &paths));
        assert!(!has_install_marker(InstallClient::Codex, &paths));
    }

    #[test]
    fn codex_uninstall_is_scoped() {
        let temp = TempDir::new().unwrap();
        let paths = test_paths(&temp);
        run_install_at(InstallClient::Claude, &paths, false).unwrap();
        run_install_at(InstallClient::Codex, &paths, false).unwrap();
        let unrelated = paths.skills_dir(InstallClient::Codex).join("my-skill");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("SKILL.md"), "keep me").unwrap();

        run_uninstall_at(InstallClient::Codex, &paths, false).unwrap();
        assert!(unrelated.join("SKILL.md").exists());
        assert!(paths.marker(InstallClient::Claude).exists());
        assert!(!paths.marker(InstallClient::Codex).exists());
        assert!(paths.claude_settings_path().exists());
    }

    #[test]
    fn claude_uninstall_preserves_other_hooks() {
        let temp = TempDir::new().unwrap();
        let paths = test_paths(&temp);
        run_install_at(InstallClient::Claude, &paths, false).unwrap();
        fs::write(paths.legacy_marker(), "0.3.0").unwrap();

        let settings_path = paths.claude_settings_path();
        let mut settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        settings["hooks"][HOOK_SKILLS[0].event]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "matcher": "Write",
                "hooks": [{"type": "command", "command": "other-tool"}]
            }));
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();

        run_uninstall_at(InstallClient::Claude, &paths, false).unwrap();
        assert!(!paths.marker(InstallClient::Claude).exists());
        assert!(!paths.legacy_marker().exists());
        let remaining: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(settings_path).unwrap()).unwrap();
        let entries = remaining["hooks"][HOOK_SKILLS[0].event].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["hooks"][0]["command"], "other-tool");
    }
}
