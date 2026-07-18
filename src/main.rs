use pingora::server::Server;
use pingora::server::configuration::ServerConf;
use pingora::services::background::background_service;
use roused::config::Config;
use roused::proxy::RousedProxy;
use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("roused: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let config_path = config_path()?;
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

fn config_path() -> Result<PathBuf, Box<dyn Error>> {
    let mut arguments = env::args_os();
    let _program = arguments.next();
    let Some(path) = arguments.next() else {
        return Err("usage: roused <config.toml>".into());
    };
    if arguments.next().is_some() {
        return Err("usage: roused <config.toml>".into());
    }
    Ok(path.into())
}
