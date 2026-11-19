//! 按需装配的配置、文件、网络与诊断服务。

mod diagnostics;
mod files;
mod network;
mod store;

pub use diagnostics::{DiagnosticConfig, DiagnosticLayer, Diagnostics, diagnostics};
pub use files::FileAccess;
pub use network::{DownloadProgress, NetworkClient, NetworkOptions, network};
pub use store::{Store, StoreConfig, store};

#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "sqlite")]
pub use sqlite::{Migration, SqliteDatabase, sqlite};
