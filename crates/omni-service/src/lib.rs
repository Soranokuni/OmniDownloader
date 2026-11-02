use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::Path;
use tracing::{error, info};


pub mod eventlog;
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
        ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept, ServiceErrorControl,
        ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState,
        ServiceStatus, ServiceType,
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
            // A service that dies here writes nothing anyone will find: the
            // file log may not even be open yet. Event Viewer is where the
            // administrator is already looking.
            crate::eventlog::report(
                crate::eventlog::EventLevel::Error,
                &format!("{SERVICE_DISPLAY_NAME} failed to run: {e:#}"),
            );
        }
    }

    /// How long SCM is told a stop may take: the daemon's own drain plus the
    /// runtime's shutdown budget, with room to spare.
    pub(crate) const STOP_WAIT_HINT: Duration = Duration::from_secs(60);

    /// What the control handler does with one control (P2.15).
    ///
    /// It runs on the dispatcher thread, which is `main`'s and so inside the
    /// `#[tokio::main]` runtime: `blocking_send` there panics, a panic cannot
    /// unwind out of the `extern "system"` callback, and the process aborts
    /// with the stop half done — SCM's caller sees "The pipe has been ended"
    /// (os error 109) and the daemon never logs that it was asked to stop.
    /// `try_send` never blocks; a second Stop while the first is queued is
    /// the same request and is dropped.
    pub(crate) fn on_control(control: ServiceControl, shutdown_tx: &tokio::sync::mpsc::Sender<()>) -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = shutdown_tx.try_send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    }

    fn stop_pending() -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::StopPending,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 1,
            wait_hint: STOP_WAIT_HINT,
            process_id: None,
        }
    }

    fn run_service_impl() -> Result<()> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
        // Set once registered: the handler reports STOP_PENDING itself, so SCM
        // waits for the drain instead of calling the service hung. Taken back
        // under the same lock before STOPPED, so a late STOP_PENDING can never
        // follow it.
        let handle_cell: std::sync::Arc<Mutex<Option<service_control_handler::ServiceStatusHandle>>> =
            Default::default();
        let handler_cell = handle_cell.clone();
        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            if matches!(control_event, ServiceControl::Stop | ServiceControl::Shutdown) {
                let cell = handler_cell.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(h) = cell.as_ref() {
                    let _ = h.set_service_status(stop_pending());
                }
            }
            on_control(control_event, &shutdown_tx)
        };

        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
            .context("Failed registering service control handler")?;
        *handle_cell.lock().unwrap_or_else(|p| p.into_inner()) = Some(status_handle);

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

        // The three moments an IT administrator looking at Event Viewer
        // actually needs (plan P6.1). Detail stays in logs/; this is the
        // breadcrumb that says where to look.
        crate::eventlog::report(
            crate::eventlog::EventLevel::Info,
            &format!("{SERVICE_DISPLAY_NAME} started."),
        );

        if let Some(run_fn) = runner {
            run_fn(shutdown_rx);
        }

        crate::eventlog::report(
            crate::eventlog::EventLevel::Info,
            &format!("{SERVICE_DISPLAY_NAME} stopped."),
        );

        let _no_more_pending = {
            let mut cell = handle_cell.lock().unwrap_or_else(|p| p.into_inner());
            cell.take();
            cell
        };
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
            .create_service(&service_info, INSTALL_ACCESS)
            .context("Failed creating Windows Service")?;

        let _ = service.set_description(SERVICE_DESCRIPTION);

        // Start after the boot rush (network shares and DNS up), and come
        // back by itself after a crash: 1 min, 1 min, 5 min, the count reset
        // after a day without one. An unattended MCR machine has nobody to
        // press "start" at 3 a.m. (plan P9).
        if let Err(e) = service.set_delayed_auto_start(true) {
            info!("Delayed auto-start not set ({e}); the service still starts with Windows");
        }
        let restart = |secs| ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(secs) };
        let actions = ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![restart(60), restart(60), restart(300)]),
        };
        if let Err(e) = service.update_failure_actions(actions) {
            info!("Restart-on-failure not set ({})", winapi_detail(&e));
        }
        // Also when the process exits with an error rather than crashing.
        if let Err(e) = service.set_failure_actions_on_non_crash_failures(true) {
            info!("Restart after a non-crash failure not set ({})", winapi_detail(&e));
        }

        // Register the Event Log source while we still have the administrator
        // rights that creating a service required. A failure here is not fatal
        // — the service will run and log to files either way — but it does mean
        // Event Viewer renders our messages wrapped in a complaint, so say so.
        if let Err(e) = crate::eventlog::register_source() {
            info!("Event Log source not registered ({e:#}); file logging is unaffected");
        }

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

    /// The handle `install` configures the new service through. Restart
    /// failure actions need SERVICE_START as well as SERVICE_CHANGE_CONFIG
    /// (ChangeServiceConfig2, SERVICE_CONFIG_FAILURE_ACTIONS); without it the
    /// call is refused with access denied and the service is never restarted
    /// after a crash (P2.15).
    pub(crate) const INSTALL_ACCESS: ServiceAccess =
        ServiceAccess::CHANGE_CONFIG.union(ServiceAccess::START);

    /// windows-service prints every OS error as "IO error in winapi call";
    /// the OS error itself is in the source.
    fn winapi_detail(e: &windows_service::Error) -> String {
        match std::error::Error::source(e) {
            Some(inner) => format!("{e}: {inner}"),
            None => e.to_string(),
        }
    }

    pub fn uninstall() -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("Failed opening Service Manager")?;

        let service = manager
            .open_service(SERVICE_NAME, ServiceAccess::DELETE | ServiceAccess::STOP)
            .context("Failed opening service for deletion")?;

        let _ = service.stop();
        service.delete().context("Failed deleting service")?;
        // Leave the machine as we found it.
        let _ = crate::eventlog::unregister_source();
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

    /// Whether this process may create services: run as administrator.
    pub fn can_install() -> bool {
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE).is_ok()
    }

    /// The executable the installed service runs, if it is installed.
    pub fn installed_exe() -> Option<std::path::PathBuf> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
        let service = manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_CONFIG).ok()?;
        Some(service.query_config().ok()?.executable_path)
    }

    pub fn installed_account() -> Option<String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
        let service = manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_CONFIG).ok()?;
        let name = service.query_config().ok()?.account_name?;
        Some(name.to_string_lossy().into_owned())
    }

    /// Stop the service if it runs, and wait until it has (up to `timeout`):
    /// the executable cannot be replaced while the process holds it.
    pub fn stop_and_wait(timeout: Duration) -> Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .context("Failed opening Service Manager")?;
        let service = manager
            .open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)
            .context("Failed opening service")?;
        if service.query_status()?.current_state == ServiceState::Stopped {
            return Ok(());
        }
        let _ = service.stop();
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if service.query_status()?.current_state == ServiceState::Stopped {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        anyhow::bail!("{SERVICE_NAME} did not stop within {} s", timeout.as_secs())
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

/// The account a new installation runs as (plan P2.16): the virtual service
/// account. Windows requires it to equal the service name, so it is the literal
/// of `NT SERVICE\` + `SERVICE_NAME` (a test keeps the two together).
pub const DEFAULT_SERVICE_ACCOUNT: &str = "NT SERVICE\\OmniIngestService";

/// Which account an install should use, before any prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountChoice {
    /// LocalSystem (`install_service` takes `None`).
    LocalSystem,
    /// An account Windows takes no password for.
    Passwordless(String),
    /// A domain or local user account; the password has to be asked for.
    NeedsPassword(String),
}

/// The one decision both installers share. Nothing given = the virtual account;
/// `LocalSystem` / `NT AUTHORITY\SYSTEM` is the explicit opt-out.
pub fn default_or_given(account: Option<&str>) -> AccountChoice {
    let Some(name) = account.map(str::trim).filter(|s| !s.is_empty()) else {
        return AccountChoice::Passwordless(DEFAULT_SERVICE_ACCOUNT.to_string());
    };
    let upper = name.to_ascii_uppercase();
    if upper == "LOCALSYSTEM" || upper == "NT AUTHORITY\\SYSTEM" || upper == "NT AUTHORITY\\LOCALSYSTEM" {
        AccountChoice::LocalSystem
    } else if ServiceAccount::is_passwordless_builtin(name) {
        AccountChoice::Passwordless(name.to_string())
    } else {
        AccountChoice::NeedsPassword(name.to_string())
    }
}

/// Whether the network-watchfolder warning applies. LocalSystem, the virtual
/// account and NetworkService all reach SMB as the computer account, which a
/// file server usually refuses; only a user account carries its own identity.
pub fn network_share_warning_applies(watchfolder: &str, choice: &AccountChoice) -> bool {
    needs_network_identity(watchfolder) && !matches!(choice, AccountChoice::NeedsPassword(_))
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

/// Whether this process may install services (it runs as administrator).
pub fn can_install_services() -> bool {
    #[cfg(windows)]
    return windows_impl::can_install();
    #[cfg(not(windows))]
    return false;
}

/// The executable the installed service points at; `None` when the
/// service is not installed.
pub fn installed_service_exe() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    return windows_impl::installed_exe();
    #[cfg(not(windows))]
    return None;
}

/// The account the installed service runs as (`LocalSystem` for the default);
/// `None` when it is not installed or cannot be read.
pub fn installed_service_account() -> Option<String> {
    #[cfg(windows)]
    return windows_impl::installed_account();
    #[cfg(not(windows))]
    return None;
}

/// Stop the service and wait until it has stopped (no-op when stopped).
pub fn stop_service_and_wait(timeout: std::time::Duration) -> Result<()> {
    #[cfg(windows)]
    return windows_impl::stop_and_wait(timeout);
    #[cfg(not(windows))]
    {
        let _ = timeout;
        return non_windows_impl::stop();
    }
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

        for name in [r"NEWSROOM\svc_omni", r".\omniservice", "svc_omni@example.org"] {
            assert!(
                !ServiceAccount::is_passwordless_builtin(name),
                "{name} is a real account and does need a password"
            );
        }
    }

    /// P2.15: SCM's Stop reaches the handler on `main`'s thread, inside the
    /// `#[tokio::main]` runtime. Blocking there panicked and aborted the
    /// process mid-stop (os error 109, no shutdown logged).
    #[cfg(windows)]
    #[tokio::test]
    async fn a_stop_inside_a_runtime_neither_panics_nor_blocks_and_reaches_the_daemon() {
        use windows_service::service::ServiceControl;
        use windows_service::service_control_handler::ServiceControlHandlerResult;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
        for c in [ServiceControl::Stop, ServiceControl::Shutdown, ServiceControl::Stop] {
            // The channel holds one: the second and third find it full and
            // must return at once rather than wait for the daemon.
            assert!(matches!(windows_impl::on_control(c, &tx), ServiceControlHandlerResult::NoError));
        }
        assert!(rx.try_recv().is_ok(), "the stop request reaches the daemon");
        assert!(matches!(
            windows_impl::on_control(ServiceControl::Interrogate, &tx),
            ServiceControlHandlerResult::NoError
        ));
        // A daemon that already returned (receiver gone) is no reason to panic either.
        drop(rx);
        assert!(matches!(windows_impl::on_control(ServiceControl::Stop, &tx), ServiceControlHandlerResult::NoError));
    }

    /// P2.15: restart-on-failure was refused at install (access denied shown
    /// only as "IO error in winapi call") because the handle lacked START.
    #[cfg(windows)]
    #[test]
    fn the_install_handle_may_set_restart_failure_actions() {
        use windows_service::service::ServiceAccess;
        assert!(windows_impl::INSTALL_ACCESS.contains(ServiceAccess::CHANGE_CONFIG));
        assert!(windows_impl::INSTALL_ACCESS.contains(ServiceAccess::START));
        assert!(windows_impl::STOP_WAIT_HINT.as_secs() >= 50, "covers the 25 s delivery wait plus the 20 s runtime shutdown");
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

    #[test]
    fn the_default_account_is_the_virtual_account_named_after_the_service() {
        assert_eq!(DEFAULT_SERVICE_ACCOUNT, format!("NT SERVICE\\{SERVICE_NAME}"));
    }

    #[test]
    fn no_account_means_the_virtual_account_and_localsystem_is_the_opt_out() {
        let virt = AccountChoice::Passwordless(DEFAULT_SERVICE_ACCOUNT.to_string());
        assert_eq!(default_or_given(None), virt);
        assert_eq!(default_or_given(Some("  ")), virt);
        for s in ["LocalSystem", "localsystem", "NT AUTHORITY\\SYSTEM", "nt authority\\localsystem"] {
            assert_eq!(default_or_given(Some(s)), AccountChoice::LocalSystem, "{s}");
        }
        assert_eq!(default_or_given(Some("DOMAIN\\svc")), AccountChoice::NeedsPassword("DOMAIN\\svc".into()));
        assert_eq!(
            default_or_given(Some("NT SERVICE\\Other")),
            AccountChoice::Passwordless("NT SERVICE\\Other".into())
        );
    }

    #[test]
    fn the_share_warning_covers_every_computer_identity_but_not_a_user_account() {
        let unc = r"\\dalet\ingest";
        assert!(network_share_warning_applies(unc, &default_or_given(None)));
        assert!(network_share_warning_applies(unc, &default_or_given(Some("LocalSystem"))));
        assert!(!network_share_warning_applies(unc, &default_or_given(Some("DOMAIN\\svc"))));
        assert!(!network_share_warning_applies(r"D:\watchfolder", &default_or_given(None)));
        assert!(!network_share_warning_applies(r"D:\watchfolder", &default_or_given(Some("LocalSystem"))));
    }
}

/// True when the watchfolder is on another machine, so the service needs an
/// identity the file server will accept.
pub fn needs_network_identity(watchfolder: &str) -> bool {
    let w = watchfolder.trim();
    w.starts_with(r"\\") || w.starts_with("//")
}
