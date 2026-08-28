// Embeds the application icon as resource ID 1, which is what the tray code
// asks LoadIconW for and what Explorer shows for the executable.
fn main() {
    let _ = embed_resource::compile("app.rc", embed_resource::NONE);
}
