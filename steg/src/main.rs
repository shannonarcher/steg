//! The `steg` binary: CLI flags, wezterm bootstrap, tokio runtime and the
//! stdio MCP transport.  All the behaviour lives in the `steg` library.
//!
//! The MCP protocol runs over stdin/stdout, so nothing in this binary may
//! ever print to stdout.  Logging goes to stderr via the regular wezterm
//! logger (`WEZTERM_LOG=steg=debug` to turn it up).

use anyhow::Context;
use clap::{Parser, ValueHint};
use config::wezterm_version;
use rmcp::ServiceExt;
use std::ffi::OsString;
use steg::{Backend, ConnectOptions, FrictionConfig, Steg};

#[derive(Debug, Parser)]
#[command(
    name = "steg",
    about = "MCP server for the WezTerm multiplexer: lets AI agents create tabs, type into panes and read the screen",
    version = wezterm_version()
)]
struct Opt {
    /// Skip loading wezterm.lua
    #[arg(long, short = 'n')]
    skip_config: bool,

    /// Specify the configuration file to use, overrides the normal
    /// configuration file resolution
    #[arg(long, value_parser, conflicts_with = "skip_config", value_hint = ValueHint::FilePath)]
    config_file: Option<OsString>,

    /// Prefer connecting to a background mux server.
    /// The default is to prefer connecting to a running wezterm gui
    /// instance, so that the agent works in a window you can see.
    #[arg(long)]
    prefer_mux: bool,

    /// When connecting to a gui instance, if you started the gui with
    /// `--class SOMETHING`, pass that same value here so that steg finds
    /// the correct gui instance.
    #[arg(long)]
    class: Option<String>,

    /// Allow steg to auto-start a background mux server when no wezterm
    /// instance is running.  Off by default: an agent should drive a
    /// terminal you can see, so steg normally reports an error and asks
    /// you to start WezTerm instead.
    #[arg(long)]
    auto_start: bool,

    /// GitHub repo (owner/repo) that `report_friction` files issues to.
    /// Defaults to $STEG_FRICTION_REPO, else the Steg repo itself.
    #[arg(long, value_name = "OWNER/REPO")]
    friction_repo: Option<String>,

    /// Remove the `report_friction` tool, so the agent cannot file issues.
    #[arg(long)]
    no_friction: bool,
}

fn main() {
    config::designate_this_as_the_main_thread();
    config::assign_error_callback(mux::connui::show_configuration_error_message);
    if let Err(err) = run() {
        log::error!("{:#}; terminating", err);
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    env_bootstrap::bootstrap();

    let opts = Opt::parse();

    config::common_init(opts.config_file.as_ref(), &[], opts.skip_config)
        .context("config::common_init")?;
    let config = config::configuration();

    let connect = ConnectOptions {
        class: opts
            .class
            .clone()
            .unwrap_or_else(|| wezterm_gui_subcommands::DEFAULT_WINDOW_CLASS.to_string()),
        prefer_mux: opts.prefer_mux,
        no_auto_start: !opts.auto_start,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("creating tokio runtime")?;

    let friction = if opts.no_friction {
        None
    } else {
        Some(FrictionConfig::defaults(opts.friction_repo.clone()))
    };

    runtime.block_on(async move {
        let backend = Backend::new(connect, config);
        let server = Steg::new(backend, friction);
        if opts.no_friction {
            log::info!("report_friction is disabled");
        } else {
            log::info!("report_friction files issues to {}", server.friction_repo());
        }

        log::info!("steg {} serving MCP over stdio", wezterm_version());
        let running = server
            .serve(rmcp::transport::stdio())
            .await
            .context("starting MCP server on stdio")?;

        let reason = running.waiting().await.context("MCP server task failed")?;
        log::info!("steg exiting: {reason:?}");
        anyhow::Ok(())
    })
}
