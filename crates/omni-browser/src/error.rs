//! What the headless browser reports when it cannot give a result.
//!
//! (Until P11.15a this lived in `agent.rs` with a placeholder for a
//! «computer-use» file-locker agent that never ran; file lockers are now
//! handled in `omni_core::locker` and the worker.)

use thiserror::Error;

#[derive(Error, Debug)]
pub enum BrowserError {
    #[error("Browser launch failed: {0}")]
    LaunchFailed(String),

    #[error("Navigation failed for {0}: {1}")]
    NavigationFailed(String, String),

    #[error("No video stream found for URL: {0}")]
    NoStreamFound(String),

    #[error("Browser timeout: {0}")]
    Timeout(String),
}
