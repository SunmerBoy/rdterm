//! `rdterm` binary shim.
//!
//! The binary only exists so the crate can be linked as an executable; all logic lives in
//! `librustdesk::cli` so that it can use crate-internal (non-`pub`) items.
//!
//! NOTE: deliberately **not** setting `windows_subsystem = "windows"` -- rdterm is a
//! console application and must keep its console attached.

fn main() {
    librustdesk::cli::run();
}
