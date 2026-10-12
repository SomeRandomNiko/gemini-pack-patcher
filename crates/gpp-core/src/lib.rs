//! Core of gemini-pack-patcher: reads a stock `GEMINI_PACK.BIN`, applies the patches
//! described in `firmware/*.toml`, writes a new pack and verifies it.
//!
//! Nothing here touches the network or the file system, so the same code runs in
//! the browser (via `gpp-wasm`) and in the CLI.

pub mod catalog;
pub mod engine;
pub mod lzo;
pub mod pack;
pub mod squashfs;

pub use catalog::{catalog, FirmwareDef, ParamDef, PatchDef};
pub use engine::{apply, identify, ApplyOutput, Check, Identification, Selection};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// `err!("...")` builds an [`Error`] with `format!` syntax.
#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => { $crate::Error(format!($($arg)*)) };
}

/// Progress callback: (stage description, fraction 0..=1 of the whole job).
pub type Progress<'a> = &'a mut dyn FnMut(&str, f32);

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(data))
}

pub(crate) fn md5_hex(data: &[u8]) -> String {
    use md5::Digest;
    hex::encode(md5::Md5::digest(data))
}
