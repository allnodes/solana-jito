mod jito;

use std::{path::PathBuf, process::Command};

type FallbackVersionGetter = fn(git_root: &PathBuf) -> String;

struct ClientVersionConfig {
    client_name: &'static str,
    is_submodule: bool,
    fallback_version_getter: Option<Box<FallbackVersionGetter>>,
}

fn set_client_version(client_name: &str, version_tag: &str) {
    println!("cargo:rustc-env=ALLNODES_CLIENT_VERSION={client_name}/{version_tag}");
}

fn main() {
    let config = jito::get_client_version_config();

    let git_root = PathBuf::from(format!(
        "{}/../../{}",
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"),
        if config.is_submodule { "../" } else { "" }
    ))
    .canonicalize()
    .expect("Failed to canonicalize Git root");

    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rerun-if-changed={}/.git/refs/tags",
        git_root.display()
    );
    if let Some(git_commit_tag) = Command::new("git")
        .current_dir(&git_root)
        .args(["describe", "--tags"])
        .output()
        .ok()
        .filter(|git_output| git_output.status.success())
        .and_then(|git_output| String::from_utf8(git_output.stdout).ok())
    {
        set_client_version(config.client_name, git_commit_tag.trim());
        return;
    }

    let fallback_version = match config.fallback_version_getter {
        Some(fallback_get_version) => fallback_get_version(&git_root),
        None => env!("CARGO_PKG_VERSION").to_owned(),
    };

    set_client_version(config.client_name, &format!("v{fallback_version}-allnodes"));
}
