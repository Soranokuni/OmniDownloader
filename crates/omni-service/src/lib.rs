use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::Path;
use tracing::{error, info};

pub const SERVICE_NAME: &str = "OmniIngestService";
pub const SERVICE_DISPLAY_NAME: &str = "OmniDownloader Broadcast Ingest Engine";
pub const SERVICE_DESCRIPTION: &str = "Automated Outlook email monitoring, Greek newsroom LLM parsing, browser video extraction, and Sony XDCAM HD422 PAL 1080i50 Dalet watchfolder ingestion.";

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;
    use windows_service::define_windows_service;
    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};


    static DAEMON_RUNNER: Mutex<Option<Box<dyn FnOnce(tokio::sync::mpsc::Receiver<()>) + Send>>> =
        Mutex::new(None);

    define_windows_service!(ffi_service_main, my_service_main);

    fn my_service_main(_arguments: Vec<OsString>) {
        if let Err(e) = run_service_impl() {
            error!("Error in service main: {:?}", e);
        }
    }

    fn run_service_impl() -> Result<()> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = shutdown_tx.blocking_send(());
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
            .context("Failed registering service control handler")?;

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        let runner = {
            let mut lock = DAEMON_RUNNER.lock().unwrap();
            lock.take()
        };

        if let Some(run_fn) = runner {
            run_fn(shutdown_rx);
        }

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        Ok(())
    }

    pub fn run_service<F>(runner: F) -> Result<()>
    where
        F: FnOnce(tokio::sync::mpsc::Receiver<()>) + Send + 'static,
    {
        {
            let mut lock = DAEMON_RUNNER.lock().unwrap();
            *lock = Some(Box::new(runner));
        }

        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .context("Failed starting service dispatcher")?;
        Ok(())
    }

    pub fn install(exe_path: &Path) -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)
            .context("Failed opening Service Manager with CREATE_SERVICE access")?;

        let service_info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_DISPLAY_NAME),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exe_path.to_path_buf(),
            launch_arguments: vec![OsString::from("run-service")],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };

        let service = manager
            .create_service(&service_info, ServiceAccess::CHANGE_CONFIG)
            .context("Failed creating Windows Service")?;

        let _ = service.set_description(SERVICE_DESCRIPTION);
        info!("Successfully installed Windows Service: {}", SERVICE_NAME);
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("Failed opening Service Manager")?;

        let service = manager
            .open_service(SERVICE_NAME, ServiceAccess::DELETE | ServiceAccess::STOP)
            .context("Failed opening service for deletion")?;

        let _ = service.stop();
        service.delete().context("Failed deleting service")?;
        info!("Successfully uninstalled Windows Service: {}", SERVICE_NAME);
        Ok(())
    }

    pub fn start() -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("Failed opening Service Manager")?;

        let service = manager
            .open_service(SERVICE_NAME, ServiceAccess::START)
            .context("Failed opening service for starting")?;

        service
            .start(&[] as &[OsString])
            .context("Failed starting service")?;
        info!("Successfully triggered start for Windows Service: {}", SERVICE_NAME);
        Ok(())
    }

    pub fn stop() -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("Failed opening Service Manager")?;

        let service = manager
            .open_service(SERVICE_NAME, ServiceAccess::STOP)
            .context("Failed opening service for stopping")?;

        service.stop().context("Failed stopping service")?;
        info!("Successfully triggered stop for Windows Service: {}", SERVICE_NAME);
        Ok(())
    }

    pub fn status() -> Result<String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("Failed opening Service Manager")?;

        let service = manager
            .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
            .context("Failed opening service for status query")?;

        let st = service.query_status().context("Failed querying service status")?;
        let state_str = match st.current_state {
            ServiceState::Running => "RUNNING",
            ServiceState::Stopped => "STOPPED",
            ServiceState::StartPending => "START_PENDING",
            ServiceState::StopPending => "STOP_PENDING",
            ServiceState::Paused => "PAUSED",
            ServiceState::PausePending => "PAUSE_PENDING",
            ServiceState::ContinuePending => "CONTINUE_PENDING",
        };
        Ok(state_str.to_string())
    }
}

#[cfg(not(windows))]
mod non_windows_impl {
    use super::*;

    pub fn run_service<F>(_runner: F) -> Result<()>
    where
        F: FnOnce(tokio::sync::mpsc::Receiver<()>) + Send + 'static,
    {
        anyhow::bail!("Windows Service execution is only supported on Windows OS.");
    }
    pub fn install(_exe_path: &Path) -> Result<()> {
        anyhow::bail!("Windows Service installation is only supported on Windows OS.");
    }
    pub fn uninstall() -> Result<()> {
        anyhow::bail!("Windows Service uninstallation is only supported on Windows OS.");
    }
    pub fn start() -> Result<()> {
        anyhow::bail!("Windows Service start is only supported on Windows OS.");
    }
    pub fn stop() -> Result<()> {
        anyhow::bail!("Windows Service stop is only supported on Windows OS.");
    }
    pub fn status() -> Result<String> {
        Ok("UNSUPPORTED_OS".into())
    }
}

pub fn run_service<F>(runner: F) -> Result<()>
where
    F: FnOnce(tokio::sync::mpsc::Receiver<()>) + Send + 'static,
{
    #[cfg(windows)]
    return windows_impl::run_service(runner);
    #[cfg(not(windows))]
    return non_windows_impl::run_service(runner);
}

pub fn install_service(exe_path: &Path) -> Result<()> {
    #[cfg(windows)]
    return windows_impl::install(exe_path);
    #[cfg(not(windows))]
    return non_windows_impl::install(exe_path);
}

pub fn uninstall_service() -> Result<()> {
    #[cfg(windows)]
    return windows_impl::uninstall();
    #[cfg(not(windows))]
    return non_windows_impl::uninstall();
}

pub fn start_service() -> Result<()> {
    #[cfg(windows)]
    return windows_impl::start();
    #[cfg(not(windows))]
    return non_windows_impl::start();
}

pub fn stop_service() -> Result<()> {
    #[cfg(windows)]
    return windows_impl::stop();
    #[cfg(not(windows))]
    return non_windows_impl::stop();
}

pub fn query_service_status() -> Result<String> {
    #[cfg(windows)]
    return windows_impl::status();
    #[cfg(not(windows))]
    return non_windows_impl::status();
}

