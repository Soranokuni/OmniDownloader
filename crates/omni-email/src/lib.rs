pub mod decontaminate;
pub mod interceptor;
pub mod llm;
pub mod mail;
pub mod parser;
pub mod watcher;

pub use decontaminate::decontaminate_email_body;
pub use interceptor::{intercept_volatile_urls, make_manual_slug};
pub use llm::{LlmClient, LlmJob, LlmParseResult};
pub use mail::{AttachmentMeta, InboundMail};
pub use parser::{parse, ParsedEmail, ParserConfig};
pub use watcher::EmailWatcher;
