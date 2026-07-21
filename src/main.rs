mod plain_http;

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use pingora::server::configuration::ServerConf;
use pingora::server::{RunArgs, Server, ShutdownSignal, ShutdownSignalWatch};
use pingora::services::background::background_service;
use pingora::services::listening::Service;
use plain_http::PlainHttpApp;
use roused::config::Config;
use roused::proxy::{GatewayShutdownHandle, RousedProxy};
use roused::setup::{SetupError, gateway_plist_xml};
use std::env;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};

const GATEWAY_CLEANUP_WAIT_TIMEOUT_SECONDS: u64 = 20;
const GATEWAY_GRACE_PERIOD_SECONDS: u64 = 1;
const GATEWAY_RUNTIME_SHUTDOWN_TIMEOUT_SECONDS: u64 = 0;

struct GatewayShutdownSignals {
    shutdown: GatewayShutdownHandle,
}

impl GatewayShutdownSignals {
    async fn coordinate_cleanup(&self, signal: &str) {
        self.shutdown.begin_shutdown();
        log::info!("gateway received {signal}; starting coordinated shutdown");
        if self
            .shutdown
            .wait_for_cleanup(Duration::from_secs(GATEWAY_CLEANUP_WAIT_TIMEOUT_SECONDS))
            .await
        {
            log::info!("gateway shutdown cleanup finished; handing shutdown to Pingora");
        } else {
            log::warn!(
                "gateway shutdown cleanup exceeded the {}-second coordination limit; handing shutdown to Pingora",
                GATEWAY_CLEANUP_WAIT_TIMEOUT_SECONDS
            );
        }
    }
}

#[async_trait]
impl ShutdownSignalWatch for GatewayShutdownSignals {
    async fn recv(&self) -> ShutdownSignal {
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut interrupt = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        let mut quit = signal(SignalKind::quit()).expect("install SIGQUIT handler");

        tokio::select! {
            _ = terminate.recv() => {
                self.coordinate_cleanup("SIGTERM").await;
                ShutdownSignal::GracefulTerminate
            }
            _ = interrupt.recv() => {
                self.coordinate_cleanup("SIGINT").await;
                ShutdownSignal::GracefulTerminate
            }
            _ = quit.recv() => {
                self.coordinate_cleanup("SIGQUIT").await;
                log::info!("starting Pingora graceful-upgrade shutdown");
                ShutdownSignal::GracefulUpgrade
            },
        }
    }
}

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

        /// Existing absolute log directory; defaults to $HOME/Library/Logs
        #[arg(long, value_name = "DIRECTORY")]
        log_dir: Option<PathBuf>,

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
                log_dir,
                program,
            }),
        ) => init_gateway_plist(&label, &config, &output, log_dir, program),
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
    log_dir: Option<PathBuf>,
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
    let uses_default_log_dir = log_dir.is_none();
    let log_dir = match log_dir {
        Some(log_dir) => log_dir,
        None => {
            let home = env::var_os("HOME").filter(|home| !home.is_empty()).ok_or(
                "cannot determine the default log directory because HOME is not set; pass --log-dir",
            )?;
            let home = PathBuf::from(home);
            if !home.is_absolute() {
                return Err(
                    "cannot determine the default log directory because HOME is not absolute; pass --log-dir"
                        .into(),
                );
            }
            home.join("Library/Logs")
        }
    };

    Config::load(config_path)?;
    let contents = gateway_plist_xml(label, &program, config_path, &log_dir).map_err(
        |error| -> Box<dyn Error> {
            if uses_default_log_dir
                && matches!(
                    &error,
                    SetupError::LogDirectoryUnavailable { .. }
                        | SetupError::LogDirectoryNotDirectory { .. }
                        | SetupError::PathNotUtf8 {
                            name: "log directory",
                            ..
                        }
                        | SetupError::InvalidXmlCharacter {
                            name: "log directory",
                            ..
                        }
                )
            {
                format!("{error}; pass --log-dir to select another directory").into()
            } else {
                Box::new(error)
            }
        },
    )?;
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
        grace_period_seconds: Some(GATEWAY_GRACE_PERIOD_SECONDS),
        graceful_shutdown_timeout_seconds: Some(GATEWAY_RUNTIME_SHUTDOWN_TIMEOUT_SECONDS),
        ..ServerConf::default()
    };
    // HTTP/1.1 only; no HTTP/2 listener behavior is enabled.
    server_config.threads = 1;

    let listen = config.listen();
    let proxy = RousedProxy::new(&config);
    let shutdown = proxy.shutdown_handle();
    let idle_monitor = background_service("idle shutdown", proxy.idle_monitor());
    let mut server = Server::new_with_opt_and_conf(None, server_config);
    server.bootstrap();

    let proxy = pingora::proxy::http_proxy(&server.configuration, proxy);
    let mut service = Service::new(
        "Pingora HTTP Proxy Service".to_owned(),
        PlainHttpApp::new(proxy),
    );
    service.add_tcp(&listen.to_string());
    server.add_service(service);
    server.add_service(idle_monitor);
    server.run(RunArgs {
        shutdown_signal: Box::new(GatewayShutdownSignals { shutdown }),
    });
    Ok(())
}
