pub mod decontaminate;
pub mod interceptor;
pub mod llm;
pub mod watcher;

pub use decontaminate::decontaminate_email_body;
pub use interceptor::{intercept_volatile_urls, make_manual_slug};
pub use llm::{LlmClient, LlmJob, LlmParseResult};
pub use watcher::EmailWatcher;
