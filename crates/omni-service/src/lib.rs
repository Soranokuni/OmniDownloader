use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::Path;
use tracing::{error, info};

pub const SERVICE_NAME: &str = "OmniIngestService";
pub const SERVICE_DISPLAY_NAME: &str = "OmniDownloader Broadcast Ingest Engine";
pub const SERVICE_DESCRIPTION: &str = "Automated mailbox monitoring, newsroom rundown parsing, browser video extraction, and Sony XDCAM HD422 PAL 1080i50 delivery to the broadcast ingest watchfolder.";

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

    pub fn install(exe_path: &Path, account: Option<&ServiceAccount>) -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)
            .context("Failed opening Service Manager with CREATE_SERVICE access")?;

        // `None` means LocalSystem, which is fine for a single machine writing
        // to a local watchfolder — and wrong the moment the watchfolder is a
        // UNC share, because LocalSystem authenticates to SMB as the *computer
        // account*, which a file server will usually refuse (defect W-13). The
        // caller warns about that; here we just honour what it decided.
        let (account_name, account_password) = match account {
            Some(a) => (
                Some(OsString::from(&a.name)),
                a.password.as_ref().map(OsString::from),
            ),
            None => (None, None),
        };

        let service_info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_DISPLAY_NAME),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exe_path.to_path_buf(),
            launch_arguments: vec![OsString::from("run-service")],
            dependencies: vec![],
            account_name,
            account_password,
        };

        let service = manager
            .create_service(&service_info, ServiceAccess::CHANGE_CONFIG)
            .context("Failed creating Windows Service")?;

        let _ = service.set_description(SERVICE_DESCRIPTION);
        match account {
            Some(a) => info!(
                "Successfully installed Windows Service {} running as {}",
                SERVICE_NAME, a.name
            ),
            None => info!(
                "Successfully installed Windows Service {} running as LocalSystem",
                SERVICE_NAME
            ),
        }
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

/// The account a service runs as (plan P2.8, defect W-13).
///
/// `password: None` covers the accounts Windows does not take a password for:
/// the virtual service account `NT SERVICE\OmniIngestService`, and the built-in
/// `NT AUTHORITY\NetworkService` / `LocalService`.
#[derive(Debug, Clone)]
pub struct ServiceAccount {
    pub name: String,
    pub password: Option<String>,
}

impl ServiceAccount {
    /// A domain or local account, with its password.
    pub fn with_password(name: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            password: Some(password.into()),
        }
    }

    /// True for the built-in accounts that take no password.
    pub fn is_passwordless_builtin(name: &str) -> bool {
        let n = name.trim().to_ascii_uppercase();
        n.starts_with("NT SERVICE\\")
            || n == "NT AUTHORITY\\NETWORKSERVICE"
            || n == "NT AUTHORITY\\LOCALSERVICE"
            || n == "NT AUTHORITY\\LOCALSYSTEM"
            || n == "LOCALSYSTEM"
    }
}

/// Install the service. `account` of `None` means LocalSystem.
pub fn install_service(exe_path: &Path, account: Option<&ServiceAccount>) -> Result<()> {
    #[cfg(windows)]
    return windows_impl::install(exe_path, account);
    #[cfg(not(windows))]
    {
        let _ = account;
        return non_windows_impl::install(exe_path);
    }
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_accounts_that_take_no_password_are_recognised() {
        // Prompting for a password for one of these and then passing an empty
        // string makes CreateService fail with a misleading "logon failure".
        for name in [
            r"NT SERVICE\OmniIngestService",
            r"nt service\omniingestservice",
            r"NT AUTHORITY\NetworkService",
            r"NT AUTHORITY\LocalService",
            "LocalSystem",
        ] {
            assert!(
                ServiceAccount::is_passwordless_builtin(name),
                "{name} should not be prompted for a password"
            );
        }

        for name in [r"CRETETV\svc_omni", r".\omniservice", "svc_omni@cretetv.gr"] {
            assert!(
                !ServiceAccount::is_passwordless_builtin(name),
                "{name} is a real account and does need a password"
            );
        }
    }

    #[test]
    fn a_unc_watchfolder_is_what_makes_localsystem_wrong() {
        // LocalSystem authenticates to SMB as the computer account, which a
        // file server usually refuses -- so delivery fails at the last step,
        // after a correct MXF has already been produced (defect W-13).
        assert!(needs_network_identity(r"\\dalet\ingest"));
        assert!(needs_network_identity(r"//dalet/ingest"));
        assert!(!needs_network_identity(r"D:\watchfolder"));
        assert!(!needs_network_identity("watchfolder"));
    }
}

/// True when the watchfolder is on another machine, so the service needs an
/// identity the file server will accept.
pub fn needs_network_identity(watchfolder: &str) -> bool {
    let w = watchfolder.trim();
    w.starts_with(r"\\") || w.starts_with("//")
}
