//! Download engine shared by the app, the command-line tool and the browser host.
//! A port of HoardiniCore (macOS); behaviour follows SPEC.md (range probe, segmenting, resume rules).

pub mod downloader;
pub mod error;
pub mod fmp4;
pub mod hls;
pub mod job;
pub mod net;
pub mod pair;
pub mod paths;
pub mod probe;
pub mod remux;
pub mod segments;

pub use downloader::{DownloadProgress, Downloader, ProgressFn};
pub use error::{Error, Result};
pub use hls::HlsDownloader;
pub use job::JobRequest;
pub use pair::PairDownloader;

/// Called once a download's final file name is settled, before any bytes are written.
pub type DestinationFn = std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync>;
