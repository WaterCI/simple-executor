use anyhow::Result;
use clap::Parser;
use rmp_serde::decode::Error;
use rmp_serde::{Deserializer, Serializer};
use sentry;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::net::TcpStream;
use std::path::Path;
use std::process::exit;
use tracing::{debug, error, info, instrument, span, Level};
use tracing_subscriber;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};
use waterlib::executors::simple_docker::{SimpleDockerExecutor, SimpleDockerExecutorConfig};
use waterlib::executors::Executor;
use waterlib::net::ExecutorMessage::ExecutorRegister;
use waterlib::net::{ExecutorMessage, ExecutorStatus, JobBuildRequestMessage};

#[derive(Deserialize, Debug)]
struct Config {
    #[serde(default = "Config::default_core_host")]
    core_host: String,
    #[serde(default = "Config::default_core_port")]
    core_port: u32,
}
impl Config {
    fn default_core_host() -> String {
        if let Ok(host) = env::var("WATERCI_CORE_HOST") {
            return host;
        }
        "127.0.0.1".to_string()
    }
    fn default_core_port() -> u32 {
        if let Ok(port) = env::var("WATERCI_CORE_PORT") {
            match port.parse::<u32>() {
                Ok(i) => {
                    return i;
                }
                Err(_) => {}
            }
        }
        5633
    }
}
impl Default for Config {
    fn default() -> Self {
        Self {
            core_host: Config::default_core_host(),
            core_port: Config::default_core_port(),
        }
    }
}

fn get_config(path: &str) -> anyhow::Result<Config> {
    let p = Path::new(path);
    if p.exists() {
        let mut f = File::open(p)?;
        let mut s = String::new();
        f.read_to_string(&mut s)?;
        let c = serde_yaml::from_str(&s)?;
        return Ok(c);
    }
    Ok(Config::default())
}

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = "Sync server for water-ci")]
struct Args {
    #[clap(short, long, default_value = "docker-executor.waterci.yml")]
    config_file: String,
}

#[instrument]
fn try_init_sentry() -> Option<sentry::ClientInitGuard> {
    tracing_subscriber::Registry::default()
        .with(sentry::integrations::tracing::layer())
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();
    if let Ok(dsn) = env::var("SENTRY_DSN") {
        return Some(sentry::init((
            dsn,
            sentry::ClientOptions {
                release: sentry::release_name!(),
                traces_sample_rate: 1.0,
                ..Default::default()
            },
        )));
    }
    None
}

#[instrument]
fn main() {
    let _guard = try_init_sentry();
    let args = Args::parse();
    let conf = get_config(&args.config_file).expect("Could not read config file");
    match run(conf) {
        Ok(_) => {}
        Err(e) => {
            error!("Error while running: {e}");
            exit(1);
        }
    };
}

#[instrument]
fn run(config: Config) -> Result<()> {
    let mut stream = TcpStream::connect(format!("{}:{}", &config.core_host, config.core_port))?;
    debug!("Connected to server, trying to register");
    ExecutorRegister.serialize(&mut Serializer::new(&mut stream))?;
    debug!("Sent register request, waiting on response");
    let resp = ExecutorMessage::deserialize(&mut Deserializer::new(&mut stream))
        .expect("could not read register response");
    let uid = if let ExecutorMessage::ExecutorRegisterResponse { id } = resp {
        id
    } else {
        panic!("Invalid response from server: {resp:?}");
    };
    info!("Successfully connected and logged in the core server as executor {uid}");
    loop {
        let span = span!(Level::TRACE, "executor_mainloop", uid = uid.as_str());
        let _enter = span.enter();
        debug!("Waiting on message from core…");
        match ExecutorMessage::deserialize(&mut Deserializer::new(&mut stream)) {
            Ok(msg) => {
                debug!("Got message from core: {msg:?}");
                match msg {
                    ExecutorMessage::BuildRequest(req) => {
                        info!("Got build request from core: {req:?}");
                        let mut executor = SimpleDockerExecutor::new(SimpleDockerExecutorConfig {});
                        for job in req.repo_config.jobs {
                            let job_build_request_message = JobBuildRequestMessage {
                                repo_url: req.repo_url.clone(),
                                reference: req.reference.clone(),
                                job,
                            };

                            let res = executor.execute(job_build_request_message)?;
                            ExecutorMessage::JobResult(res).write(&mut stream)?;
                        }
                    }
                    ExecutorMessage::ExecutorStatusQuery => {
                        ExecutorMessage::ExecutorStatusResponse(ExecutorStatus::Available)
                            .write(&mut stream)?;
                    }
                    ExecutorMessage::CloseConnection(_uid) => {
                        break;
                    }
                    _ => {
                        panic!("invalid message from core: {msg:?}");
                    }
                }
            }
            Err(e) => match e {
                Error::InvalidMarkerRead(e) => match e.kind() {
                    ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::UnexpectedEof => {
                        error!("Core has disconnected");
                        break;
                    }
                    kind => {
                        panic!("Unhandled error while reading core message: {kind:?}");
                    }
                },
                _ => {
                    panic!("Error communicating with core: {e}");
                }
            },
        }
    }
    Ok(())
}
