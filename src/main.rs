use clap::{Parser, Subcommand};
use pingora::server::Server;
use pingora::server::configuration::ServerConf;
use pingora::services::background::background_service;
use roused::config::Config;
use roused::proxy::RousedProxy;
use roused::setup::gateway_plist_xml;
use std::env;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "roused",
    version,
    about = "Activation reverse proxy for macOS LaunchAgents",
    override_usage = "roused <config.toml>\n       roused <COMMAND>",
    args_conflicts_with_subcommands = true,
    arg_required_else_help = true
)]
struct Cli {
    /// Run the gateway with this configuration file
    #[arg(value_name = "CONFIG.TOML")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Write a schema-derived starter configuration
    InitConfig {
        #[arg(value_name = "OUTPUT.TOML")]
        output: PathBuf,
    },

    /// Validate a configuration without starting the gateway
    CheckConfig {
        #[arg(value_name = "CONFIG.TOML")]
        config: PathBuf,
    },

    /// Generate the Roused gateway LaunchAgent plist
    InitGatewayPlist {
        /// launchd label for the Roused gateway
        #[arg(long, value_name = "LABEL")]
        label: String,

        /// Absolute path to the Roused configuration
        #[arg(long, value_name = "CONFIG.TOML")]
        config: PathBuf,

        /// Absolute path for the generated plist
        #[arg(long, value_name = "OUTPUT.PLIST")]
        output: PathBuf,

        /// Absolute Roused executable path; defaults to this executable
        #[arg(long, value_name = "ROUSED")]
        program: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if error.use_stderr() => {
            eprint!("roused: {error}");
            return ExitCode::from(2);
        }
        Err(help) => {
            print!("{help}");
            return ExitCode::SUCCESS;
        }
    };

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("roused: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match (cli.config, cli.command) {
        (Some(config), None) => run_gateway(config),
        (None, Some(CliCommand::InitConfig { output })) => init_config(&output),
        (None, Some(CliCommand::CheckConfig { config })) => check_config(&config),
        (
            None,
            Some(CliCommand::InitGatewayPlist {
                label,
                config,
                output,
                program,
            }),
        ) => init_gateway_plist(&label, &config, &output, program),
        (None, None) => unreachable!("clap requires a configuration path or subcommand"),
        (Some(_), Some(_)) => unreachable!("clap rejects mixed runtime and subcommand arguments"),
    }
}

fn init_config(output: &Path) -> Result<(), Box<dyn Error>> {
    let contents = Config::starter_toml()
        .map_err(|error| format!("cannot generate starter configuration: {error}"))?;
    Config::from_toml(&contents)
        .map_err(|error| format!("generated starter configuration is invalid: {error}"))?;
    write_new(output, contents.as_bytes())?;

    println!("created starter configuration at {}", output.display());
    println!(
        "Next: edit its [[services]] entries, then run `roused check-config {}`.",
        output.display()
    );
    Ok(())
}

fn check_config(config_path: &Path) -> Result<(), Box<dyn Error>> {
    Config::load(config_path)?;
    println!("configuration is valid: {}", config_path.display());
    Ok(())
}

fn init_gateway_plist(
    label: &str,
    config_path: &Path,
    output: &Path,
    program: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    if !output.is_absolute() {
        return Err("--output must be an absolute path".into());
    }
    let program = match program {
        Some(program) => program,
        None => env::current_exe()
            .map_err(|error| format!("cannot determine the current executable path: {error}"))?,
    };

    Config::load(config_path)?;
    let contents = gateway_plist_xml(label, &program, config_path)?;
    write_new(output, contents.as_bytes())?;

    println!("created gateway LaunchAgent plist at {}", output.display());
    println!(
        "Next: run `/usr/bin/plutil -lint {}` before bootstrapping it.",
        output.display()
    );
    Ok(())
}

fn write_new(path: &Path, contents: &[u8]) -> Result<(), Box<dyn Error>> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    file.write_all(contents)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(())
}

fn run_gateway(config_path: PathBuf) -> Result<(), Box<dyn Error>> {
    let config = Config::load(&config_path)?;

    let mut server_config = ServerConf {
        // Pingora treats this field as the total attempt cap, despite its name.
        max_retries: 1,
        ..ServerConf::default()
    };
    // HTTP/1.1 only; no HTTP/2 listener behavior is enabled.
    server_config.threads = 1;

    let listen = config.listen();
    let proxy = RousedProxy::new(&config);
    let idle_monitor = background_service("idle shutdown", proxy.idle_monitor());
    let mut server = Server::new_with_opt_and_conf(None, server_config);
    server.bootstrap();

    let mut service = pingora::proxy::http_proxy_service(&server.configuration, proxy);
    service.add_tcp(&listen.to_string());
    server.add_service(service);
    server.add_service(idle_monitor);
    server.run_forever();
}
