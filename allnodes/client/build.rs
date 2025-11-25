use std::process::Command;

const CLIENT_VERSION_ENV_VAR: &str = "ALLNODES_CLIENT_VERSION";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/refs/tags");
    println!("cargo:rerun-if-env-changed={CLIENT_VERSION_ENV_VAR}");
    if let Ok(client_version) = std::env::var(CLIENT_VERSION_ENV_VAR) {
        println!("cargo:rustc-env={CLIENT_VERSION_ENV_VAR}={client_version}");
        return;
    }
    if let Ok(git_output) = Command::new("git").args(["describe", "--tags"]).output() {
        if git_output.status.success() {
            if let Ok(git_commit_tag) = String::from_utf8(git_output.stdout) {
                println!(
                    "cargo:rustc-env={CLIENT_VERSION_ENV_VAR}={}",
                    git_commit_tag.trim()
                );
            }
        }
    } else {
        panic!(
            "Git tag is not set for this commit. Please define git commit tag or set \
            `{CLIENT_VERSION_ENV_VAR}` environment variable."
        )
    }
}
