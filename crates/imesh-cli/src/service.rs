//! The daemon as a Windows service (`imesh daemon windows-service`), started by the Service
//! Control Manager at boot. `imesh daemon install --system` registers it.

use std::{ffi::OsString, sync::OnceLock, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use imesh::daemon;
use tracing::error;
use windows_service::{
    define_windows_service,
    service::{ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType},
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

use crate::{args::DaemonArgs, cli::Register, install, paths};

const NAME: &str = "imesh";

static REGISTER: OnceLock<Register> = OnceLock::new();

/// Reads the daemon's settings from the environment, as `imesh daemon` does.
#[derive(Parser)]
struct Settings {
    #[command(flatten)]
    args: DaemonArgs,
}

/// Hands the process to the Service Control Manager, which calls [`service_main`].
pub fn run(register: Register) -> Result<()> {
    // The service's own places, then its settings file. Only this thread runs yet.
    let data = paths::system_data_dir();
    for (k, v) in [
        ("IMESH_DATA_DIR", data.display().to_string()),
        ("IMESH_SOCKET", paths::SYSTEM_SOCKET.to_string()),
        ("IMESH_SOCKET_GROUP", paths::SYSTEM_GROUP.to_string()),
    ] {
        // SAFETY: no other thread runs yet.
        unsafe { std::env::set_var(k, v) };
    }
    install::load_env(&data.join("env"));
    let _ = REGISTER.set(register);
    service_dispatcher::start(NAME, ffi_service_main)
        .context("start as a Windows service (run by the Service Control Manager only)")
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_: Vec<OsString>) {
    if let Err(e) = serve() {
        error!("service: {e:#}");
    }
}

fn status(state: ServiceState, exit: u32) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(exit),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    }
}

fn serve() -> Result<()> {
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let handle = service_control_handler::register(NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            stop.send_replace(true);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    handle.set_service_status(status(ServiceState::Running, 0))?;
    let result = (|| {
        let args = Settings::try_parse_from([NAME])?.args;
        let register = REGISTER.get().context("no registration")?.clone();
        let config = args.config();
        let mut stopped = stopped;
        tokio::runtime::Runtime::new()?.block_on(daemon::serve(
            || register(daemon::Builder::new(config.clone())),
            &args.local_api(),
            async move {
                let _ = stopped.wait_for(|s| *s).await;
            },
        ))
    })();
    // A non-zero exit code makes the Service Control Manager apply the restart actions.
    handle.set_service_status(status(ServiceState::Stopped, if result.is_ok() { 0 } else { 1 }))?;
    result
}
