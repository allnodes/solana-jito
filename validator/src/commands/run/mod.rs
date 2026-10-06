pub mod args;
pub mod execute;
pub mod tip_router;
#[cfg(target_os = "linux")]
pub mod caps_check;
#[cfg(target_os = "linux")]
pub mod seccomp;
#[cfg(target_os = "linux")]
pub mod xdp_receive;

pub use {args::add_args, execute::execute};

pub struct Config {
    #[cfg(target_os = "linux")]
    pub primordial_caps: caps::CapsHashSet,
}
