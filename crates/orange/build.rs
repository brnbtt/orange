fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=../orange-tray/icon.ico");
    embed_resource::compile("app.rc", embed_resource::NONE)
        .manifest_required()
        .expect("the Windows viewer resources must compile");
}
