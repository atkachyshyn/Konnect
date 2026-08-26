mod config;
mod install;
mod manifest;
mod transaction_cli;
mod transport;

use anyhow::Result;
use config::{Config, TransportMode};
use konnect_core::mcp::handler::McpHandler;
use std::io::IsTerminal;
use tracing::info;
use tracing_subscriber::{fmt, EnvFilter};

/// What an invocation means, decided from the arguments alone.
///
/// Pulled out of `main` so it can be tested: the whole of #238 was a dispatch
/// mistake, and a test that has to run the binary to find one would install
/// into the tester's real home — `dirs::home_dir()` on Windows calls
/// `SHGetKnownFolderPath` and ignores `HOME`/`USERPROFILE`.
#[derive(Debug, PartialEq, Eq)]
enum Cli<'a> {
    /// Print help for a subcommand, or for the program when `None`.
    Help(Option<&'a str>),
    Version,
    Subcommand(&'a str),
    /// Run as an MCP server (or, on a terminal, the double-click install).
    Server,
}

/// Subcommands that take `--client`/`--help` rather than being server flags.
const SUBCOMMANDS: &[&str] = &["init", "uninstall", "status", "skill", "transaction"];

fn classify(args: &[String]) -> Cli<'_> {
    let subcommand = args
        .get(1)
        .map(String::as_str)
        .filter(|name| SUBCOMMANDS.contains(name));

    // `--help` is answered wherever it appears, and before anything can write.
    // It used to be one arm of a match the subcommands were listed above, so
    // `konnect init --help` never reached it: `init` matched first and
    // `client_from_args` discarded the `--help` (#238). Help is never an
    // action.
    if args
        .iter()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        return Cli::Help(subcommand);
    }
    match args.get(1).map(String::as_str) {
        Some("help") => Cli::Help(args.get(2).map(String::as_str)),
        Some("--version") | Some("-V") => Cli::Version,
        Some(name) if SUBCOMMANDS.contains(&name) => Cli::Subcommand(name),
        _ => Cli::Server,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // ─── CLI argument parsing (minimal, no clap dependency) ─────────
    let args: Vec<String> = std::env::args().collect();

    // ─── Subcommand dispatch (install, uninstall, status, skill) ────
    match classify(&args) {
        Cli::Help(subcommand) => {
            print_help_for(subcommand);
            return Ok(());
        }
        Cli::Version => {
            println!("konnect {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Cli::Subcommand("init") => {
            let client = install::client_from_args(&args[2..])?;
            return install::run_install(client);
        }
        Cli::Subcommand("uninstall") => {
            let client = install::client_from_args(&args[2..])?;
            return install::run_uninstall(client);
        }
        Cli::Subcommand("status") => {
            let client = install::client_from_args(&args[2..])?;
            return install::print_status(client);
        }
        Cli::Subcommand("skill") => {
            let name = args.get(2).map(String::as_str).unwrap_or("");
            return install::print_skill_content(name);
        }
        Cli::Subcommand("transaction") => return transaction_cli::run(&args[2..]),
        Cli::Subcommand(_) | Cli::Server => {}
    }

    // ─── Double-click detection ─────────────────────────────────────
    // If stdin is a terminal (user double-clicked the .exe), run friendly install.
    // If stdin is piped (Claude launched us as MCP server), start server.
    if std::io::stdin().is_terminal() {
        return install::run_double_click_install();
    }

    let client = install::client_from_server_args(&args[1..])?;
    let eager_override = install::eager_toolsets_from_server_args(&args[1..])?;
    let dispatcher_override = install::dispatcher_tools_from_server_args(&args[1..])?;

    // ─── Auto-install on first MCP launch (safety net) ──────────────
    if install::needs_install(client) {
        let _ = install::run_install_silent(client);
    }

    // --config <path>: load config from specified file
    let config_path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|pos| args.get(pos + 1))
        .map(std::path::PathBuf::from);

    let config = if let Some(ref path) = config_path {
        // KiCAD launches the server this way (with KICAD_API_SOCKET set), so
        // the env fallback for a blank ipc_address must apply here too (#39).
        let mut c = Config::load_from(path)?;
        c.apply_env_fallbacks();
        c
    } else {
        Config::load()?
    };

    // ─── Initialize tracing (stderr only — stdout is MCP protocol) ──
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level));

    fmt::Subscriber::builder()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();

    info!("Konnect v{} starting", env!("CARGO_PKG_VERSION"));

    // Codex caches the tool list from the first `tools/list` and never acts on
    // `notifications/tools/list_changed`, so anything outside the starter kit
    // would stay uncallable there for the whole session (#134, #169). The
    // dispatcher tools cure that without enlarging the listing; eager loading
    // cures it too but costs roughly ten times the context, so it stays opt-in.
    // Config and the explicit CLI switches override both.
    let eager_toolsets = config.eager_toolsets_for(eager_override);
    let dispatcher_tools =
        config.dispatcher_tools_for(client.caches_initial_tool_list(), dispatcher_override);
    info!(
        client = %client,
        eager_toolsets,
        dispatcher_tools,
        "tool exposure: {}",
        match (eager_toolsets, dispatcher_tools) {
            (true, _) => "all toolsets pre-loaded at startup",
            (false, true) => "starter kit + dispatcher tools (whole catalogue reachable on demand)",
            (false, false) => "starter kit only, expand with load_toolset",
        }
    );

    let server_config = konnect_core::tools::ServerConfig {
        kicad_cli: config.kicad_cli.clone(),
        kicad_binary: config.kicad_binary.clone(),
        ipc_address: config.ipc_address.clone(),
        project_dir: config.project_dir.clone(),
        jlcpcb_db_path: config.jlcpcb_db_path.clone(),
        auto_load_toolsets: config.auto_load_toolsets,
        eager_toolsets,
        dispatcher_tools,
    };
    let handler = McpHandler::new(server_config).await?;

    match config.transport {
        TransportMode::Stdio => {
            transport::stdio::run_stdio(handler).await?;
        }
        TransportMode::Http => {
            transport::http::run_http(handler, &config.http_address).await?;
        }
        TransportMode::Both => {
            let handler_http = handler.clone();
            let http_addr = config.http_address.clone();
            let http_task = tokio::spawn(async move {
                transport::http::run_http(handler_http, &http_addr)
                    .await
                    .expect("HTTP transport failed");
            });
            let stdio_task = tokio::spawn(async move {
                transport::stdio::run_stdio(handler)
                    .await
                    .expect("STDIO transport failed");
            });
            tokio::select! {
                _ = http_task => {},
                _ = stdio_task => {},
            }
        }
    }

    Ok(())
}

/// Help for one subcommand, or the whole program.
fn print_help_for(subcommand: Option<&str>) {
    let clients = || {
        println!("\nCLIENTS:");
        println!("  claude (default)         Skills, agents, and hooks under ~/.claude");
        println!("  codex                    Skills under ~/.agents/skills");
    };
    match subcommand {
        Some("init") => {
            println!("konnect init [--client <claude|codex>]\n");
            println!("Install Konnect's skills and agents for an AI client.");
            println!("Writes to that client's configuration directory.");
            clients();
        }
        Some("uninstall") => {
            println!("konnect uninstall [--client <claude|codex>]\n");
            println!("Remove the skills and agents Konnect installed for a client.");
            println!("Only Konnect's own named assets are removed.");
            clients();
        }
        Some("status") => {
            println!("konnect status [--client <claude|codex>]\n");
            println!("Report what is installed for a client. Writes nothing.");
            clients();
        }
        Some("skill") => {
            println!("konnect skill <name>\n");
            println!("Print a bundled skill's content to stdout, for hooks. Writes nothing.");
        }
        Some("transaction") => {
            println!("konnect transaction status <project-dir>");
            println!("konnect transaction recover <project-dir>");
            println!("konnect transaction abandon <project-dir> <transaction-id> --force\n");
            println!("Inspect and resolve write-ahead journals left by multi-file");
            println!("schematic changes. 'status' writes nothing.");
        }
        _ => print_help(),
    }
}

fn print_help() {
    println!("Konnect v{}", env!("CARGO_PKG_VERSION"));
    println!("MCP server for KiCad EDA with embedded guidance.\n");
    println!("USAGE:");
    println!("  konnect                  Start MCP server (pipe) or install (TTY)");
    println!("  konnect [--client <client>] [--config <path>]");
    println!("           [--dispatcher-tools | --no-dispatcher-tools]");
    println!("           [--eager-toolsets | --no-eager-toolsets]");
    println!("  konnect init [--client <client>]");
    println!("  konnect uninstall [--client <client>]");
    println!("  konnect status [--client <client>]");
    println!("  konnect skill <name>     Print skill content (for hooks)");
    println!("  konnect transaction status <project-dir>");
    println!("  konnect transaction recover <project-dir>");
    println!("  konnect transaction abandon <project-dir> <id> --force");
    println!("  konnect --version        Print version");
    println!("  konnect --help           This message");
    println!("\nTOOL EXPOSURE:");
    println!("  --dispatcher-tools       Expose list_available_tools,");
    println!("                           get_tool_schema and execute_konnect_tool,");
    println!("                           which reach every tool on demand (~1K)");
    println!("  --no-dispatcher-tools    Omit them");
    println!("  --eager-toolsets         Pre-load every toolset so the first");
    println!("                           tools/list is complete (~34K tokens)");
    println!("  --no-eager-toolsets      Starter kit only (~2K tokens); expand");
    println!("                           on demand with load_toolset");
    println!();
    println!("  Dispatcher tools default on for codex, which caches its first");
    println!("  tools/list and so cannot use load_toolset; off for claude, which");
    println!("  refreshes and does not need them. Eager loading is off for both --");
    println!("  it reaches the same tools for ~10x the context. Config keys");
    println!("  override the defaults, and these flags override config.");
    println!("\nCLIENTS:");
    println!("  claude (default)         Skills, agents, and hooks under ~/.claude");
    println!("  codex                    Skills under ~/.agents/skills");
}

#[cfg(test)]
mod cli_dispatch_tests {
    use super::*;

    fn cli(argv: &[&str]) -> Vec<String> {
        std::iter::once("konnect")
            .chain(argv.iter().copied())
            .map(String::from)
            .collect()
    }

    /// The whole of #238: `konnect init --help` ran the Claude installer and
    /// rewrote `~/.claude`, because the `init` arm was matched before the
    /// `--help` arm and `client_from_args` then discarded the `--help`.
    ///
    /// `uninstall --help` was the same code path. The reporter did not run it.
    #[test]
    fn subcommand_help_is_help_and_never_the_subcommand() {
        for name in ["init", "uninstall", "status", "skill", "transaction"] {
            for flag in ["--help", "-h"] {
                assert_eq!(
                    classify(&cli(&[name, flag])),
                    Cli::Help(Some(name)),
                    "konnect {name} {flag}"
                );
            }
        }
    }

    /// Including when it trails other arguments, which is where a user who has
    /// half-written a command actually puts it.
    #[test]
    fn help_is_answered_wherever_it_appears() {
        assert_eq!(
            classify(&cli(&["init", "--client", "codex", "--help"])),
            Cli::Help(Some("init"))
        );
        assert_eq!(
            classify(&cli(&["--client", "codex", "--help"])),
            Cli::Help(None)
        );
        assert_eq!(
            classify(&cli(&[
                "transaction",
                "abandon",
                "dir",
                "id",
                "--force",
                "-h"
            ])),
            Cli::Help(Some("transaction"))
        );
    }

    #[test]
    fn the_subcommands_still_dispatch_without_a_help_flag() {
        assert_eq!(classify(&cli(&["init"])), Cli::Subcommand("init"));
        assert_eq!(
            classify(&cli(&["init", "--client", "codex"])),
            Cli::Subcommand("init")
        );
        assert_eq!(
            classify(&cli(&["skill", "konnect"])),
            Cli::Subcommand("skill")
        );
        assert_eq!(
            classify(&cli(&["transaction", "status", "dir"])),
            Cli::Subcommand("transaction")
        );
        assert_eq!(classify(&cli(&["--version"])), Cli::Version);
        assert_eq!(classify(&cli(&["-V"])), Cli::Version);
        assert_eq!(classify(&cli(&["help"])), Cli::Help(None));
        assert_eq!(classify(&cli(&["help", "init"])), Cli::Help(Some("init")));
    }

    /// Everything else is a server launch, including the `--config` form the
    /// bundled `examples/*.json` tell users to write.
    #[test]
    fn anything_else_starts_the_server() {
        assert_eq!(classify(&cli(&[])), Cli::Server);
        assert_eq!(
            classify(&cli(&["--config", "C:/konnect.json"])),
            Cli::Server
        );
        assert_eq!(classify(&cli(&["--client", "codex"])), Cli::Server);
    }
}
