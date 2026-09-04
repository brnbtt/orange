// Embeds the application icon as resource ID 1, which is what the tray code
// asks LoadIconW for and what Explorer shows for the executable.
fn main() {
    println!("cargo:rerun-if-env-changed=ORANGE_BUILD_ID");
    println!("cargo:rerun-if-env-changed=ORANGE_UPDATE_CHANNEL");
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=icon.ico");
    embed_resource::compile("app.rc", embed_resource::NONE)
        .manifest_required()
        .expect("the Windows application resources must compile");
}
