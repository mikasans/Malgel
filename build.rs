//! Embeds the app icon and version information in the Windows executable.
//! Other targets need neither, so for them this does nothing.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=packaging/windows/malgel.ico");

    // The commit, for log lines and crash reports (absent outside a git
    // checkout, e.g. a crates.io build).
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty());
    if let Some(sha) = sha {
        println!("cargo:rustc-env=MALGEL_GIT_SHA={sha}");
    }
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Icon resource 1 is also what GPUI loads as the window icon. File and
    // product versions come from Cargo.toml.
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon("packaging/windows/malgel.ico")
        .set("ProductName", "Malgel")
        .set("FileDescription", "Malgel")
        .set("InternalName", "malgel")
        .set("OriginalFilename", "Malgel.exe")
        .set("CompanyName", "Ross Cawston")
        .set(
            "LegalCopyright",
            "Copyright 2026 Ross Cawston. Apache License 2.0.",
        );
    if let Err(err) = resource.compile() {
        panic!("could not embed the Windows icon and version information: {err}");
    }
}
