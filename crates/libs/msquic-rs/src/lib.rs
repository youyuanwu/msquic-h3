#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    clippy::derivable_impls,
    clippy::missing_safety_doc,
    clippy::too_many_arguments,
    clippy::extra_unused_lifetimes,
    clippy::useless_transmute
)]
pub mod Microsoft;

// In linux force to pull in pal lib for linking
#[cfg(target_os = "linux")]
extern crate mssf_pal;

//#![link_args = "-Wl,-rpath /path/to/c_lib"]

#[cfg(target_os = "windows")]
mod win_status;
#[cfg(target_os = "windows")]
pub use win_status::*;

#[cfg(target_os = "linux")]
mod unix_status;
#[cfg(target_os = "linux")]
pub use unix_status::*;

mod load;
pub use load::{MsQuicClose, MsQuicOpenVersion};
