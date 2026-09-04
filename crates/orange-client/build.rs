// Embeds the application icon as resource ID 1, which is what the client code
// asks LoadIconW for and what Explorer shows for the executable, plus the
// version block Task Manager and the file Properties dialog read.
//
// app.rc is a template. Its placeholders are filled in here and the result is
// written to OUT_DIR, rather than passing `/D` defines to the resource
// compiler: those have to survive both Cargo's argument handling and rc.exe's
// own quoting, and a string value that needs embedded quotes does not.
// Substitution keeps the version stated once, in Cargo.toml, so ship.ps1's
// bump cannot leave the resource behind.
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=ORANGE_BUILD_ID");
    println!("cargo:rerun-if-env-changed=ORANGE_UPDATE_CHANNEL");
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=../../assets/icon.ico");

    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets this"));

    let version = env!("CARGO_PKG_VERSION");
    // FILEVERSION wants four comma-separated numbers. The crate version has
    // three, and an omitted fourth field is a resource compiler error rather
    // than a default.
    let commas = {
        let mut parts: Vec<&str> = version.split('.').collect();
        parts.resize(4, "0");
        parts.join(",")
    };

    // The generated file lives outside the crate, so the icon needs an
    // absolute path. Backslashes are escapes inside an .rc string literal.
    // The icon is shared with the `orange` crate, so it lives in the workspace
    // `assets/` directory rather than in either crate.
    let workspace = manifest
        .parent()
        .and_then(|crates| crates.parent())
        .expect("the crate lives two directories below the workspace root");
    let icon = workspace
        .join("assets")
        .join("icon.ico")
        .display()
        .to_string()
        .replace('\\', "\\\\");

    let template =
        std::fs::read_to_string(manifest.join("app.rc")).expect("app.rc must be readable");
    let generated = out.join("app.rc");
    std::fs::write(
        &generated,
        template
            .replace("@ICON@", &icon)
            .replace("@VERSION_COMMAS@", &commas)
            .replace("@VERSION@", version),
    )
    .expect("the generated resource script must be writable");

    embed_resource::compile(&generated, embed_resource::NONE)
        .manifest_required()
        .expect("the Windows application resources must compile");
}
