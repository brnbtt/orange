//! A real system client icon.
//!
//! GPUI has no client support, so this is raw Win32: a hidden message-only
//! window owns the notification icon and receives its callbacks. It runs on
//! its own thread because a window's message loop must live on the thread
//! that created it.
//!
//! Clicks are reported back through a channel rather than touching the UI
//! directly, so the GPUI side stays in charge of its own state.

use anyhow::{Context, Result};
use std::cell::{Cell, RefCell};
use std::io::Write;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE,
    HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcessId, OpenEventW, SetEvent, WaitForSingleObject,
    EVENT_MODIFY_STATE,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const FINAL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const DESTROY_ATTEMPTS: u32 = 20;
const MESSAGE_WAIT_MS: u32 = 1_000;

const ID_SHOW: usize = 1;
const ID_QUIT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientEvent {
    /// Left click, or "Open" from the menu.
    Show,
    Quit,
}

static TRAY_CLASS_RESULT: OnceLock<std::result::Result<(), u32>> = OnceLock::new();
static TRAY_MESSAGE_RESULT: OnceLock<std::result::Result<u32, u32>> = OnceLock::new();

struct ClientContext {
    events: RefCell<Option<Sender<ClientEvent>>>,
    icon_added: Cell<bool>,
    cleaned: Cell<bool>,
    shutdown_event: isize,
}

impl ClientContext {
    fn disconnect_events(&self) {
        self.events.borrow_mut().take();
    }

    unsafe fn cleanup(&self, hwnd: HWND) {
        if self.cleaned.replace(true) {
            return;
        }
        self.disconnect_events();
        if self.icon_added.replace(false) {
            let data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }
}

struct OwnedIcon(Option<HICON>);

impl Drop for OwnedIcon {
    fn drop(&mut self) {
        if let Some(icon) = self.0.take() {
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
    }
}

/// Owns one client window, its events, and its native message-loop thread.
pub struct Client {
    hwnd: isize,
    events: Receiver<ClientEvent>,
    worker: Option<JoinHandle<std::result::Result<(), String>>>,
    shutdown_event: Option<OwnedHandle>,
    cleanup_error: Option<String>,
}

impl Client {
    /// Install the client icon and start its native message loop.
    pub fn install() -> Result<Self> {
        Self::install_inner(true)
    }

    fn install_inner(add_icon: bool) -> Result<Self> {
        let (event_tx, events) = channel();
        let (ready_tx, ready_rx) = channel::<Result<isize>>();
        // SAFETY: CreateEventW returns a new owning handle. from_raw_handle
        // transfers that sole ownership into OwnedHandle exactly once.
        let shutdown_event = unsafe {
            let handle = CreateEventW(None, true, false, PCWSTR::null())?;
            OwnedHandle::from_raw_handle(handle.0)
        };
        let shutdown_handle = shutdown_event.as_raw_handle() as isize;
        let worker = std::thread::spawn(move || {
            // SAFETY: The owner keeps shutdown_handle open until this worker
            // terminates, and client_worker owns all native window operations.
            unsafe { client_worker(event_tx, ready_tx, add_icon, shutdown_handle) }
        });

        match ready_rx.recv_timeout(SHUTDOWN_TIMEOUT) {
            Ok(Ok(hwnd)) => Ok(Self {
                hwnd,
                events,
                worker: Some(worker),
                shutdown_event: Some(shutdown_event),
                cleanup_error: None,
            }),
            Ok(Err(error)) => {
                finish_setup_worker(worker, &shutdown_event, FINAL_SHUTDOWN_TIMEOUT);
                Err(error)
            }
            Err(error) => {
                finish_setup_worker(worker, &shutdown_event, FINAL_SHUTDOWN_TIMEOUT);
                match error {
                    RecvTimeoutError::Timeout => {
                        anyhow::bail!("client thread was not ready within {SHUTDOWN_TIMEOUT:?}")
                    }
                    RecvTimeoutError::Disconnected => {
                        Err(error).context("client thread died before it was ready")
                    }
                }
            }
        }
    }

    /// Receive a pending client action without blocking.
    pub fn try_recv(&self) -> std::result::Result<ClientEvent, TryRecvError> {
        self.events.try_recv()
    }

    /// Remove the icon and window on their creator thread, then join it.
    ///
    /// # Errors
    ///
    /// Returns an error without consuming the worker handle if shutdown does
    /// not complete before the deadline. A later call may retry.
    pub fn shutdown(&mut self) -> Result<()> {
        self.shutdown_with_timeout(SHUTDOWN_TIMEOUT)
    }

    pub(crate) fn shutdown_final(&mut self) -> Result<()> {
        self.shutdown_with_timeout(FINAL_SHUTDOWN_TIMEOUT)
    }

    fn shutdown_with_timeout(&mut self, timeout: Duration) -> Result<()> {
        let Some(worker) = self.worker.as_ref() else {
            return match &self.cleanup_error {
                Some(error) => Err(anyhow::anyhow!(error.clone())),
                None => Ok(()),
            };
        };
        if worker.thread().id() == std::thread::current().id() {
            anyhow::bail!("cannot join client worker from itself");
        }

        let deadline = Instant::now() + timeout;
        if !wait_for_thread(worker, Duration::ZERO)? {
            let shutdown_event = self
                .shutdown_event
                .as_ref()
                .expect("live worker retains its shutdown event");
            signal_event(shutdown_event)?;
            if let Ok(message) = unsafe { client_message() } {
                if let Err(error) = unsafe {
                    PostMessageW(
                        Some(HWND(self.hwnd as *mut _)),
                        message,
                        WPARAM(0),
                        LPARAM(0),
                    )
                } {
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "[client] modal wake post failed; event wait remains active: {error}"
                    );
                }
            }
        }
        match wait_for_thread(worker, deadline.saturating_duration_since(Instant::now()))? {
            true => self.join_finished_worker(),
            false => anyhow::bail!("client worker did not stop within {timeout:?}"),
        }
    }

    fn join_finished_worker(&mut self) -> Result<()> {
        let worker = self
            .worker
            .take()
            .expect("worker remains owned until its native handle is signaled");
        debug_assert!(worker.is_finished());
        match worker.join() {
            Ok(Ok(())) => {
                self.hwnd = 0;
                self.shutdown_event.take();
                Ok(())
            }
            Ok(Err(error)) => {
                self.cleanup_error = Some(error.clone());
                Err(anyhow::anyhow!(error))
            }
            Err(_) => {
                let error = "client worker panicked before proving native cleanup".to_string();
                self.cleanup_error = Some(error.clone());
                Err(anyhow::anyhow!(error))
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown_final() {
            fail_fast("client Drop could not complete native cleanup", &error);
        }
    }
}

fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

fn signal_event(event: &OwnedHandle) -> Result<()> {
    // SAFETY: OwnedHandle keeps this valid event handle open for the call.
    unsafe { SetEvent(raw_handle(event))? };
    Ok(())
}

fn wait_for_thread<T>(worker: &JoinHandle<T>, timeout: Duration) -> Result<bool> {
    let timeout_ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    // SAFETY: AsRawHandle borrows the native thread handle. `worker` remains
    // alive and unmoved for the complete wait, so Windows cannot observe a
    // closed handle. Thread handles are valid waitable synchronization objects.
    let result = unsafe { WaitForSingleObject(HANDLE(worker.as_raw_handle()), timeout_ms) };
    if result == WAIT_OBJECT_0 {
        Ok(true)
    } else if result == WAIT_TIMEOUT {
        Ok(false)
    } else if result == WAIT_FAILED {
        Err(anyhow::anyhow!(
            "waiting for client thread failed (Win32 error {})",
            unsafe { GetLastError().0 }
        ))
    } else {
        Err(anyhow::anyhow!(
            "waiting for client thread returned unexpected status {result:?}"
        ))
    }
}

fn finish_setup_worker(
    worker: JoinHandle<std::result::Result<(), String>>,
    event: &OwnedHandle,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    signal_event(event).unwrap_or_else(|error| fail_fast("could not stop client setup", &error));
    match wait_for_thread(&worker, deadline.saturating_duration_since(Instant::now())) {
        Ok(true) => {}
        Ok(false) => fail_fast(
            "client setup did not terminate before the final deadline",
            &anyhow::anyhow!("timeout after {timeout:?}"),
        ),
        Err(error) => fail_fast("could not wait for client setup", &error),
    }
    match worker.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => fail_fast("client setup cleanup failed", &anyhow::anyhow!(error)),
        Err(_) => fail_fast(
            "client setup worker panicked",
            &anyhow::anyhow!("native client ownership may be incomplete"),
        ),
    }
}

fn retry_error_if_owned<E>(
    result: std::result::Result<(), E>,
    owns: impl FnOnce() -> bool,
) -> Option<E> {
    match result {
        Ok(()) => None,
        Err(error) if owns() => Some(error),
        Err(_) => None,
    }
}

pub(crate) fn fail_fast(context: &str, error: &anyhow::Error) -> ! {
    let _ = writeln!(std::io::stderr().lock(), "[client] {context}: {error:#}");
    std::process::abort();
}

unsafe fn ensure_client_class(instance: HINSTANCE) -> Result<()> {
    let result = TRAY_CLASS_RESULT.get_or_init(|| {
        let class = WNDCLASSW {
            lpfnWndProc: Some(client_proc),
            hInstance: instance,
            lpszClassName: w!("orange_client_icon"),
            ..Default::default()
        };
        if RegisterClassW(&class) != 0 {
            Ok(())
        } else {
            Err(GetLastError().0)
        }
    });
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            anyhow::bail!("failed to register client window class (Win32 error {error})")
        }
    }
}

unsafe fn client_message() -> Result<u32> {
    let result = TRAY_MESSAGE_RESULT.get_or_init(|| {
        let message = RegisterWindowMessageW(w!(
            "orange_client_callback_88dd87b7-0a96-42aa-babd-fc9841a93f71"
        ));
        if message == 0 {
            Err(GetLastError().0)
        } else {
            Ok(message)
        }
    });
    result.as_ref().copied().map_err(|error| {
        anyhow::anyhow!("failed to register client callback (Win32 error {error})")
    })
}

fn is_client_message(message: u32) -> bool {
    TRAY_MESSAGE_RESULT
        .get()
        .and_then(|result| result.as_ref().ok())
        .is_some_and(|registered| *registered == message)
}

unsafe fn client_worker(
    events: Sender<ClientEvent>,
    ready_tx: Sender<Result<isize>>,
    add_icon: bool,
    shutdown_handle: isize,
) -> std::result::Result<(), String> {
    let context = Box::new(ClientContext {
        events: RefCell::new(Some(events)),
        icon_added: Cell::new(false),
        cleaned: Cell::new(false),
        shutdown_event: shutdown_handle,
    });
    let mut hwnd = None;
    let mut owned_icon = None;
    match create(&context, add_icon, &mut hwnd, &mut owned_icon) {
        Ok(()) => {
            let window = hwnd.expect("successful client setup has an HWND");
            // Auto-reset and initially unset: every signal wakes the loop
            // exactly once. Created after the window exists, so a signal can
            // never arrive before there is something to show.
            let show_event = CreateEventW(None, false, false, show_event_name()).ok();
            if ready_tx.send(Ok(window.0 as isize)).is_ok() {
                if let Err(error) =
                    run_message_loop(window, HANDLE(shutdown_handle as *mut _), show_event)
                {
                    let _ = writeln!(std::io::stderr().lock(), "[client] {error}");
                }
            }
            if let Some(event) = show_event {
                let _ = CloseHandle(event);
            }
        }
        Err(error) => {
            let _ = ready_tx.send(Err(error));
        }
    }

    if let Some(hwnd) = hwnd {
        if let Err(error) = destroy_window(hwnd, &context) {
            context.disconnect_events();
            std::mem::forget(owned_icon);
            Box::leak(context);
            return Err(error.to_string());
        }
    }
    context.disconnect_events();
    drop(owned_icon);
    drop(context);
    Ok(())
}

unsafe fn create(
    context: &ClientContext,
    add_icon: bool,
    hwnd: &mut Option<HWND>,
    owned_icon: &mut Option<OwnedIcon>,
) -> Result<()> {
    let instance = GetModuleHandleW(None)?;
    let class_name = w!("orange_client_icon");
    ensure_client_class(instance.into())?;

    // HWND_MESSAGE creates a message-only window: no pixels, never shown.
    let window = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class_name,
        w!("orange"),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        Some(instance.into()),
        None,
    )?;
    *hwnd = Some(window);

    let icon = match LoadImageW(
        Some(instance.into()),
        PCWSTR(std::ptr::with_exposed_provenance(1)),
        IMAGE_ICON,
        GetSystemMetrics(SM_CXSMICON),
        GetSystemMetrics(SM_CYSMICON),
        LR_DEFAULTCOLOR,
    ) {
        Ok(handle) => {
            let icon = HICON(handle.0);
            *owned_icon = Some(OwnedIcon(Some(icon)));
            icon
        }
        Err(_) => {
            let icon = LoadIconW(None, IDI_APPLICATION)?;
            *owned_icon = Some(OwnedIcon(None));
            icon
        }
    };

    SetLastError(ERROR_SUCCESS);
    let context_ptr = context as *const ClientContext as isize;
    let previous = SetWindowLongPtrW(window, GWLP_USERDATA, context_ptr);
    if previous == 0 {
        let error = GetLastError();
        if error != ERROR_SUCCESS {
            anyhow::bail!("failed to install client context (Win32 error {})", error.0);
        }
    } else {
        SetWindowLongPtrW(window, GWLP_USERDATA, 0);
        let _ = writeln!(
            std::io::stderr().lock(),
            "[client] replaced unexpected existing window context"
        );
        anyhow::bail!("client window unexpectedly had an existing context");
    }

    let callback_message = client_message()?;
    if !add_icon {
        return Ok(());
    }

    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: callback_message,
        // Load the optical small-size entry from the multi-resolution icon;
        // asking for the system client metric avoids a blurry 32px downscale.
        hIcon: icon,
        ..Default::default()
    };

    let tip: Vec<u16> = "orange".encode_utf16().chain(std::iter::once(0)).collect();
    data.szTip[..tip.len()].copy_from_slice(&tip);

    // Mark first so even synchronous destruction during the shell call takes
    // the matching delete path. Deleting an icon that was not added is safe.
    context.icon_added.set(true);
    if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
        anyhow::bail!("Shell_NotifyIcon refused to add the client icon");
    }
    Ok(())
}

unsafe fn run_message_loop(
    hwnd: HWND,
    shutdown_event: HANDLE,
    show_event: Option<HANDLE>,
) -> std::result::Result<(), String> {
    // Shutdown is always index 0. The show event is optional: without it the
    // client still works, a second launch just cannot raise this window.
    let handles: Vec<HANDLE> = match show_event {
        Some(show) => vec![shutdown_event, show],
        None => vec![shutdown_event],
    };
    loop {
        let wait = MsgWaitForMultipleObjectsEx(
            Some(&handles),
            MESSAGE_WAIT_MS,
            QS_ALLINPUT,
            MWMO_INPUTAVAILABLE,
        );
        if wait == WAIT_OBJECT_0 {
            return Ok(());
        }
        if show_event.is_some() && wait.0 == WAIT_OBJECT_0.0 + 1 {
            // A second launch asked us to surface. The event auto-resets, so
            // there is nothing to clear. Emitting Show lands this on the same
            // path as the client menu's Open rather than a second one.
            emit(hwnd, ClientEvent::Show);
            continue;
        }
        if wait.0 == WAIT_OBJECT_0.0 + handles.len() as u32 {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);

                match WaitForSingleObject(shutdown_event, 0) {
                    status if status == WAIT_OBJECT_0 => return Ok(()),
                    status if status == WAIT_TIMEOUT => {}
                    status if status == WAIT_FAILED => {
                        return Err(format!(
                            "checking shutdown event failed (Win32 error {})",
                            GetLastError().0
                        ));
                    }
                    status => {
                        return Err(format!(
                            "checking shutdown event returned unexpected status {status:?}"
                        ));
                    }
                }
            }
            continue;
        }
        if wait == WAIT_FAILED {
            return Err(format!(
                "message wait failed (Win32 error {})",
                GetLastError().0
            ));
        }
        if wait == WAIT_TIMEOUT {
            continue;
        }
        return Err(format!("message wait returned unexpected status {wait:?}"));
    }
}

unsafe fn context_still_owned(hwnd: HWND, expected: *const ClientContext) -> bool {
    SetLastError(ERROR_SUCCESS);
    let actual = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ClientContext;
    if actual.is_null() {
        let error = GetLastError();
        if error != ERROR_SUCCESS {
            let _ = writeln!(
                std::io::stderr().lock(),
                "[client] could not verify failed-destroy HWND ownership (Win32 error {})",
                error.0
            );
        }
        return false;
    }
    actual == expected
}

unsafe fn destroy_window(hwnd: HWND, context: &ClientContext) -> Result<()> {
    let expected = context as *const ClientContext;
    let mut last_error = None;
    for attempt in 1..=DESTROY_ATTEMPTS {
        let result = DestroyWindow(hwnd);
        let Some(error) = retry_error_if_owned(result, || context_still_owned(hwnd, expected))
        else {
            return Ok(());
        };
        let _ = writeln!(
            std::io::stderr().lock(),
            "[client] owned HWND DestroyWindow attempt {attempt} failed: {error}"
        );
        last_error = Some(error);
        if attempt == DESTROY_ATTEMPTS {
            break;
        }

        let wait = MsgWaitForMultipleObjectsEx(None, 100, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        if wait == WAIT_FAILED {
            let _ = writeln!(
                std::io::stderr().lock(),
                "[client] cleanup message wait failed (Win32 error {})",
                GetLastError().0
            );
        }
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message != WM_QUIT {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if !context_still_owned(hwnd, expected) {
            return Ok(());
        }
    }
    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("failed to destroy owned client HWND")))
}

/// Runs `action` synchronously with the context installed for this HWND.
///
/// # Safety
///
/// The HWND must be accessed on its creator thread. The action must not destroy
/// the window or dispatch reentrant messages. A non-null `GWLP_USERDATA` must
/// be the live `ClientContext` allocation installed by `create`.
unsafe fn with_context<R>(
    hwnd: HWND,
    action: impl for<'a> FnOnce(&'a ClientContext) -> R,
) -> Option<R> {
    let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ClientContext;
    // SAFETY: The caller upholds the pointer validity and reentrancy contract.
    context.as_ref().map(action)
}

unsafe fn emit(hwnd: HWND, event: ClientEvent) {
    let _ = with_context(hwnd, |context| {
        if let Some(events) = context.events.borrow().as_ref() {
            let _ = events.send(event);
        }
    });
}

extern "system" fn client_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            message if is_client_message(message) => {
                let shutdown = with_context(hwnd, |context| {
                    let status = WaitForSingleObject(HANDLE(context.shutdown_event as *mut _), 0);
                    let error = (status == WAIT_FAILED).then(|| GetLastError().0);
                    (status, error)
                });
                match shutdown {
                    Some((status, _)) if status == WAIT_OBJECT_0 => {
                        let _ = DestroyWindow(hwnd);
                        return LRESULT(0);
                    }
                    Some((status, _)) if status == WAIT_TIMEOUT => {}
                    Some((status, error)) => {
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "[client] callback event wait returned {status:?}{}",
                            error
                                .map(|error| format!(" (Win32 error {error})"))
                                .unwrap_or_default()
                        );
                        return LRESULT(0);
                    }
                    None => return DefWindowProcW(hwnd, msg, wparam, lparam),
                }
                // The mouse message arrives in the low word of lparam.
                match (lparam.0 as u32) & 0xFFFF {
                    x if x == WM_LBUTTONUP => emit(hwnd, ClientEvent::Show),
                    x if x == WM_RBUTTONUP => show_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match wparam.0 & 0xFFFF {
                    ID_SHOW => emit(hwnd, ClientEvent::Show),
                    ID_QUIT => emit(hwnd, ClientEvent::Quit),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ClientContext;
                SetLastError(ERROR_SUCCESS);
                let previous = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if previous == 0 {
                    let error = GetLastError();
                    if error != ERROR_SUCCESS {
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "[client] failed to clear client context (Win32 error {})",
                            error.0
                        );
                    }
                }
                // SAFETY: The worker owns this allocation until after
                // DestroyWindow returns and the HWND is confirmed dead.
                if let Some(context) = context.as_ref() {
                    context.cleanup(hwnd);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                // Retry a failed WM_DESTROY clear while the worker-owned
                // context is still live. Cleanup itself is idempotent.
                let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ClientContext;
                if !context.is_null() {
                    SetLastError(ERROR_SUCCESS);
                    let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    if let Some(context) = context.as_ref() {
                        context.cleanup(hwnd);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Holds this process's claim to being *the* running instance. A mutex lives
/// only as long as a handle to it is open, so the claim has to outlive `main`;
/// releasing it would let the next launch start a second copy.
static INSTANCE_CLAIM: OnceLock<OwnedHandle> = OnceLock::new();

/// True when another copy is already running, in which case it has been asked
/// to show its window and this process should exit without opening anything.
///
/// Clicking a pinned taskbar icon launches a fresh process whenever the app has
/// no taskbar button - which is exactly the state the close button leaves it in,
/// since it hides the window rather than quitting. Without this guard every such
/// click started a rival process with its own client icon, all of them writing the
/// same preferences file.
pub fn defer_to_running_instance() -> bool {
    // `Local\` scopes the claim to the logon session, so two users on one
    // machine still get an instance each.
    defer_to_named(
        w!("Local\\orange-instance-88dd87b7-0a96-42aa-babd-fc9841a93f71"),
        &INSTANCE_CLAIM,
    )
}

fn defer_to_named(name: PCWSTR, claim: &OnceLock<OwnedHandle>) -> bool {
    // SAFETY: CreateMutexW returns a new owning handle, which moves into
    // OwnedHandle exactly once. GetLastError is read immediately afterwards,
    // before any other call can overwrite it.
    unsafe {
        let Ok(handle) = CreateMutexW(None, false, name) else {
            // The claim could not be made at all. Running a second copy is a
            // smaller failure than refusing to start.
            return false;
        };
        let already_running = GetLastError() == ERROR_ALREADY_EXISTS;
        let handle = OwnedHandle::from_raw_handle(handle.0);
        if already_running {
            wake_running_instance();
            return true;
        }
        let _ = claim.set(handle);
        false
    }
}

/// Name of the event the running instance waits on to be told to surface.
fn show_event_name() -> PCWSTR {
    w!("Local\\orange-show-88dd87b7-0a96-42aa-babd-fc9841a93f71")
}

/// Ask the instance holding the claim to bring its window up.
///
/// This signals a named event rather than posting to the running instance's
/// window. Its client window is message-only, and `FindWindowEx` cannot resolve
/// that class name from another process - measured, not assumed: the window is
/// there and enumerable as a child of the message-only parent, but every name
/// lookup returns null, including one handed that parent explicitly. A named
/// event has no such ambiguity, and `OpenMutexW` already proves named kernel
/// objects cross the process boundary here.
unsafe fn wake_running_instance() {
    // A background process cannot take the foreground on its own. This one was
    // just launched by the user and can hand that right over; without a window
    // its pid is unknown, so the permission is granted broadly.
    let _ = AllowSetForegroundWindow(ASFW_ANY);
    signal_show_event(show_event_name());
}

/// Signal the named show event. False when nothing is listening on it.
fn signal_show_event(name: PCWSTR) -> bool {
    // SAFETY: OpenEventW returns a new owning handle, moved into OwnedHandle
    // exactly once, which closes it on the way out.
    unsafe {
        let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, name) else {
            return false;
        };
        let event = OwnedHandle::from_raw_handle(event.0);
        SetEvent(HANDLE(event.as_raw_handle())).is_ok()
    }
}

/// Hide the main window entirely, leaving the app alive in the client.
///
/// GPUI exposes `minimize` but no per-window hide, so this goes through Win32.
/// The handle is found by enumerating our own top-level windows and cached,
/// because once hidden the window is no longer discoverable by visibility.
pub fn hide_main_window() {
    if let Some(hwnd) = main_window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Bring the main window back and focus it.
pub fn show_main_window() {
    if let Some(hwnd) = main_window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

static MAIN_WINDOW: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

fn main_window() -> Option<HWND> {
    if let Some(handle) = MAIN_WINDOW.get() {
        return Some(HWND(*handle as *mut _));
    }
    let found = unsafe { find_main_window() }?;
    let _ = MAIN_WINDOW.set(found.0 as isize);
    Some(found)
}

unsafe extern "system" fn find_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let out = &mut *(lparam.0 as *mut Option<HWND>);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != GetCurrentProcessId() || !IsWindowVisible(hwnd).as_bool() {
        return windows::core::BOOL(1);
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return windows::core::BOOL(1);
    }
    // The message-only client window has no size; the UI window does.
    if rect.right - rect.left > 100 && rect.bottom - rect.top > 100 {
        *out = Some(hwnd);
        return windows::core::BOOL(0);
    }
    windows::core::BOOL(1)
}

unsafe fn find_main_window() -> Option<HWND> {
    let mut found: Option<HWND> = None;
    let _ = EnumWindows(
        Some(find_proc),
        LPARAM(&mut found as *mut Option<HWND> as isize),
    );
    found
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, ID_SHOW, w!("Open orange"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit"));

    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    // Required, or the menu refuses to close when clicking elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_RIGHTALIGN | TPM_BOTTOMALIGN,
        point.x,
        point.y,
        Some(0),
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
}

#[cfg(test)]
mod tests {
    use super::{client_message, retry_error_if_owned, Client};
    use std::cell::Cell;
    use std::os::windows::io::AsRawHandle;
    use std::process::Command;
    use windows::Win32::Foundation::{HANDLE, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM};
    use windows::Win32::System::Threading::WaitForSingleObject;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowThreadProcessId, PostThreadMessageW, SendMessageTimeoutW, SMTO_ABORTIFHUNG,
        WM_QUIT,
    };

    const LIFECYCLE_CHILD: &str = "ORANGE_TRAY_LIFECYCLE_CHILD";

    #[test]
    fn a_show_signal_crosses_the_process_boundary_by_name() {
        // The regression this guards: the first version of this looked the
        // running instance up with FindWindowEx by class name, which returns
        // null for a message-only window created by another process even when
        // handed the correct parent. Nothing failed loudly - a second launch
        // just silently did nothing. Named kernel objects resolve where window
        // names do not, and this pins that.
        //
        // A unique name, so it never contends with a real running orange.
        let name = windows::core::w!("Local\\orange-show-test-7d21e4b8");

        // The running instance's side of the handshake.
        let listener = unsafe {
            windows::Win32::System::Threading::CreateEventW(None, false, false, name).unwrap()
        };

        // Nothing has signalled yet.
        assert_eq!(
            unsafe { WaitForSingleObject(listener, 0) },
            WAIT_TIMEOUT,
            "the event starts unsignalled"
        );

        // The second instance's side: open by name and signal.
        assert!(super::signal_show_event(name), "signalling should succeed");
        assert_eq!(
            unsafe { WaitForSingleObject(listener, 500) },
            WAIT_OBJECT_0,
            "the running instance must observe the signal"
        );

        // Auto-reset: one signal wakes the loop exactly once, so a stale set
        // state cannot make it spin.
        assert_eq!(
            unsafe { WaitForSingleObject(listener, 0) },
            WAIT_TIMEOUT,
            "the event resets itself after one wake"
        );

        unsafe { windows::Win32::Foundation::CloseHandle(listener).unwrap() };

        // Nobody listening is a clean false, not a panic.
        assert!(!super::signal_show_event(windows::core::w!(
            "Local\\orange-show-test-nobody-home"
        )));
    }

    #[test]
    fn the_second_claim_on_a_name_defers_to_the_first() {
        // A distinct name, so this never contends with a real running orange.
        let name = windows::core::w!("Local\\orange-instance-test-3f9a1c04");
        let winner = std::sync::OnceLock::new();
        let loser = std::sync::OnceLock::new();

        assert!(!super::defer_to_named(name, &winner), "first claim wins");
        assert!(
            winner.get().is_some(),
            "the winner holds the mutex open, or the next launch starts a second copy"
        );

        assert!(
            super::defer_to_named(name, &loser),
            "second claim defers to the first"
        );
        assert!(loser.get().is_none(), "the loser claims nothing");
    }

    #[test]
    fn destroy_retry_checks_identity_only_after_failure() {
        let identity_checks = Cell::new(0);
        assert_eq!(
            retry_error_if_owned(Ok::<(), &str>(()), || {
                identity_checks.set(identity_checks.get() + 1);
                true
            }),
            None
        );
        assert_eq!(identity_checks.get(), 0);

        assert_eq!(retry_error_if_owned(Err("owned"), || true), Some("owned"));
        assert_eq!(retry_error_if_owned(Err("reused"), || false), None);
        assert!(super::SHUTDOWN_TIMEOUT < super::FINAL_SHUTDOWN_TIMEOUT);
    }

    #[test]
    fn owner_shutdown_joins_promptly_and_allows_reinstall() {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "client::tests::native_lifecycle_child",
                "--test-threads=1",
            ])
            .env(LIFECYCLE_CHILD, "1")
            .spawn()
            .expect("lifecycle child should start");

        let wait = unsafe { WaitForSingleObject(HANDLE(child.as_raw_handle()), 5_000) };
        if wait == WAIT_TIMEOUT {
            let kill = child.kill();
            let reap = child.wait();
            panic!("client lifecycle child exceeded five seconds; kill={kill:?}; wait={reap:?}");
        }
        if wait != WAIT_OBJECT_0 {
            let kill = child.kill();
            let reap = child.wait();
            panic!(
                "waiting for client lifecycle child failed: {wait:?}; kill={kill:?}; wait={reap:?}"
            );
        }
        assert!(child.wait().unwrap().success());
    }

    #[test]
    #[ignore = "run in a bounded subprocess by owner_shutdown_joins_promptly_and_allows_reinstall"]
    fn native_lifecycle_child() {
        if std::env::var_os(LIFECYCLE_CHILD).is_none() {
            return;
        }

        let mut first = Client::install_inner(false).expect("first client should install");
        assert!(!first.worker.as_ref().unwrap().is_finished());
        let callback = unsafe { client_message().expect("wake message should be registered") };
        let delivered = unsafe {
            SendMessageTimeoutW(
                windows::Win32::Foundation::HWND(first.hwnd as *mut _),
                callback,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                1_000,
                None,
            )
        };
        assert_ne!(delivered.0, 0, "wake message should dispatch");
        assert!(
            !first.worker.as_ref().unwrap().is_finished(),
            "an unsignaled instance event must reject the wake"
        );
        first.shutdown().expect("first client should shut down");
        assert!(first.worker.is_none());
        assert_eq!(
            first.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        );
        first
            .shutdown()
            .expect("repeated shutdown should be harmless");

        let mut second = Client::install_inner(false).expect("second client should install");
        let thread_id = unsafe {
            GetWindowThreadProcessId(
                windows::Win32::Foundation::HWND(second.hwnd as *mut _),
                None,
            )
        };
        unsafe {
            PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0))
                .expect("WM_QUIT should post");
        }
        second
            .shutdown()
            .expect("WM_QUIT cleanup should complete and join");
        assert!(second.worker.is_none());
        assert_eq!(
            second.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        );

        let owner = std::rc::Rc::new(std::cell::RefCell::new(Some(
            Client::install_inner(false).expect("app-owned client should install"),
        )));
        crate::shutdown_owned_client(&owner).expect("app-owned client should shut down");
        assert!(owner.borrow().is_none());

        let final_owner = std::rc::Rc::new(std::cell::RefCell::new(Some(
            Client::install_inner(false).expect("final app client should install"),
        )));
        crate::finish_owned_client(&final_owner).expect("outer app owner should finish shutdown");
        assert!(final_owner.borrow().is_none());

        drop(Client::install_inner(false).expect("drop-owned client should install"));
        let mut after_drop =
            Client::install_inner(false).expect("install after Drop should succeed");
        after_drop
            .shutdown()
            .expect("post-Drop client should shut down");
    }
}
