//! Capture-only entry point, copied into a disposable worktree by prepare.py.
//! The production view and ui modules are used verbatim.
use crate::*;

fn preview(name: &str) -> std::sync::Arc<gpui::RenderImage> {
    let root = std::env::var("ORANGE_CAPTURE_ASSETS").expect("capture asset directory");
    let pixels = image::open(std::path::Path::new(&root).join(name))
        .expect("generated preview image")
        .to_rgba8();
    let (w, h) = pixels.dimensions();
    let mut bgra = pixels.into_raw();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    capture::to_image((w, h, bgra)).expect("valid preview")
}

fn state() -> Orange {
    let capture_screen = std::env::var("ORANGE_CAPTURE_SCREEN").unwrap_or_default();
    let screen = match std::env::var("ORANGE_CAPTURE_SCREEN").as_deref() {
        Ok("pick") => Screen::PickWindow,
        Ok("streaming") => Screen::Streaming,
        _ => Screen::Home,
    };
    let windows: Vec<_> = [
        (0, "Entire display", "display.exe"),
        (1, "Vector Arena - Control", "vectorarena.exe"),
        (2, "Night Circuit - Time trial", "nightcircuit.exe"),
        (3, "Blocklands - Survival", "blocklands.exe"),
        (4, "Runevale - Inventory", "runevale.exe"),
    ]
    .into_iter()
    .map(|(hwnd, title, process)| WindowTarget {
        hwnd,
        title: title.into(),
        process: process.into(),
        width: 1920,
        height: 1080,
    })
    .collect();
    let thumbnails = [
        (0, "desktop.png"),
        (1, "arena.png"),
        (2, "racing.png"),
        (3, "voxel.png"),
        (4, "rpg.png"),
    ]
    .into_iter()
    .map(|(id, file)| (id, preview(file)))
    .collect();
    let active_target = Some(windows[1].clone());
    let friends: Vec<_> = ["fragbyte", "nightshift"]
        .into_iter()
        .map(|name| session::Friend {
            id: format!("demo-{name}"),
            name: name.into(),
            avatar_url: None,
        })
        .collect();
    let contact = |name: &str, id: &str| friends::Contact {
        profile: session::Friend {
            id: id.into(),
            name: name.into(),
            avatar_url: None,
        },
        revision: "demo-request-revision".into(),
    };
    let mut friend_sync = friends::Sync::default();
    friend_sync.synced = true;
    friend_sync.snapshot.friends = friends
        .iter()
        .cloned()
        .map(|profile| friends::Contact {
            profile,
            revision: "demo-mutual-friend".into(),
        })
        .collect();
    friend_sync.snapshot.incoming = vec![contact("respawned", "100000000000000004")];
    if capture_screen == "requests" {
        friend_sync.snapshot.outgoing = vec![contact("aimassist", "100000000000000005")];
    }
    let friend_offers = if capture_screen == "add-friend" {
        vec![contact("aimassist", "100000000000000005").profile]
    } else {
        Vec::new()
    };
    Orange {
        client_available: true,
        root_focus: None,
        screen,
        session: Some(session::Session {
            name: "pixelpilot".into(),
            id: "100000000000000001".into(),
            avatar_url: None,
            token: String::new(),
        }),
        windows,
        thumbnails,
        thumbnail_job: PickerJobs::default(),
        avatar: None,
        avatar_job: AvatarJobs::default(),
        quality: 1,
        fps: 60,
        active_target,
        active_preview: Some(preview("arena.png")),
        host: None,
        watches: Vec::new(),
        logging_in: None,
        notice: None,
        server: "ws://127.0.0.1:9/ws".into(),
        picker_scroll: gpui::ScrollHandle::new(),
        settings_scroll: gpui::ScrollHandle::new(),
        friends_scroll: gpui::ScrollHandle::new(),
        update_collapsed: false,
        settings_open: [true; 3],
        copied_at: None,
        copied_code: None,
        own_codes: Vec::new(),
        viewers_seen: 2,
        friends,
        friend_offers,
        friend_accounts: std::collections::HashMap::new(),
        legacy_friends: Vec::new(),
        friend_sync,
        requests_open: capture_screen == "requests",
        friend_menu: None,
        presence: [
            (
                "demo-fragbyte".into(),
                presence::Presence::Live {
                    code: "GG-WP".into(),
                },
            ),
            ("demo-nightshift".into(), presence::Presence::Offline),
        ]
        .into_iter()
        .collect(),
        presence_job: None,
        presence_client: presence::PresenceClient::default(),
        presence_due: Instant::now() + Duration::from_secs(86400),
        presence_error: None,
        friend_avatars: std::collections::HashMap::new(),
        friend_avatar_job: FriendAvatarJobs::default(),
        logo_epoch: 0,
        animate: false,
        updates: update::UpdateController::new(),
    }
}

pub(crate) fn run() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(ui::WINDOW_WIDTH), px(ui::WINDOW_HEIGHT)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("orange-capture".into()),
                    appears_transparent: true,
                    traffic_light_position: None,
                }),
                is_resizable: false,
                ..Default::default()
            },
            |_, cx| cx.new(|_| state()),
        )
        .expect("capture window");
        cx.activate(true);
    });
}
