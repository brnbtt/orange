"""Regress titlebar dragging using real mouse input on a disposable GPUI fixture."""
import argparse
import ctypes as C
from ctypes import wintypes as W
import os
from pathlib import Path
import subprocess
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("worktree", type=Path, help="Disposable worktree prepared with capture/prepare.py")
p.add_argument("--binary", type=Path, help="Override the fixture executable for a shared build cache")
args = p.parse_args()
root = args.worktree.resolve()
if not (root / "crates/orange-client/src/capture_fixture.rs").is_file():
    raise SystemExit("Use a prepared capture fixture, never a real account session")

u = C.WinDLL("user32", use_last_error=True)
u.SetProcessDpiAwarenessContext.argtypes = [W.HANDLE]
u.SetProcessDpiAwarenessContext(C.c_void_p(-4))
for name in ["GetWindowRect", "GetClientRect"]:
    getattr(u, name).argtypes = [W.HWND, C.POINTER(W.RECT)]
u.GetWindowThreadProcessId.argtypes = [W.HWND, C.POINTER(W.DWORD)]
u.IsWindowVisible.argtypes = [W.HWND]
u.SetForegroundWindow.argtypes = [W.HWND]
u.GetForegroundWindow.argtypes = []
u.GetForegroundWindow.restype = W.HWND
u.ClientToScreen.argtypes = [W.HWND, C.POINTER(W.POINT)]
u.GetCursorPos.argtypes = [C.POINTER(W.POINT)]
u.SetCursorPos.argtypes = [C.c_int, C.c_int]
u.SendMessageW.argtypes = [W.HWND, W.UINT, W.WPARAM, W.LPARAM]
u.SendMessageW.restype = C.c_ssize_t
u.SetWindowPos.argtypes = [W.HWND, W.HWND, C.c_int, C.c_int, C.c_int, C.c_int, W.UINT]
callback_type = C.WINFUNCTYPE(W.BOOL, W.HWND, W.LPARAM)
u.EnumWindows.argtypes = [callback_type, W.LPARAM]

class MouseInput(C.Structure):
    _fields_ = [("dx", W.LONG), ("dy", W.LONG), ("data", W.DWORD), ("flags", W.DWORD), ("time", W.DWORD), ("extra", W.WPARAM)]
class KeyboardInput(C.Structure):
    _fields_ = [("vk", W.WORD), ("scan", W.WORD), ("flags", W.DWORD), ("time", W.DWORD), ("extra", W.WPARAM)]
class InputData(C.Union):
    _fields_ = [("mouse", MouseInput), ("keyboard", KeyboardInput)]
class Input(C.Structure):
    _anonymous_ = ("data",)
    _fields_ = [("type", W.DWORD), ("data", InputData)]
u.SendInput.argtypes = [W.UINT, C.POINTER(Input), C.c_int]
u.SendInput.restype = W.UINT

def mouse_button(flags):
    event = Input(type=0, mouse=MouseInput(flags=flags))
    assert u.SendInput(1, C.byref(event), C.sizeof(Input)) == 1, "SendInput failed"

def rect(hwnd):
    value = W.RECT()
    assert u.GetWindowRect(hwnd, C.byref(value))
    return value

env = os.environ.copy()
env.update(ORANGE_CAPTURE_ASSETS=str(root / "capture-art"), ORANGE_CAPTURE_SCREEN="home", APPDATA=str(root / "capture-profile"), LOCALAPPDATA=str(root / "capture-local"), ORANGE_SERVER="ws://127.0.0.1:9/ws")
process = subprocess.Popen([str(args.binary or root / "target/debug/orange-tray.exe")], env=env, cwd=root)
cursor = W.POINT(); u.GetCursorPos(C.byref(cursor))
try:
    found = []
    @callback_type
    def find(hwnd, _):
        pid = W.DWORD(); u.GetWindowThreadProcessId(hwnd, C.byref(pid))
        if pid.value == process.pid and u.IsWindowVisible(hwnd): found.append(hwnd)
        return True
    deadline = time.monotonic() + 10
    while not found and time.monotonic() < deadline:
        assert process.poll() is None, "Fixture exited before creating its window"
        u.EnumWindows(find, 0)
        time.sleep(.1)
    assert found, "No fixture window"
    hwnd = found[0]
    # Windows can reject activation from a background test runner until Alt
    # releases its foreground lock, even though the fixture painted normally.
    for flags in [0, 2]:
        event = Input(type=1, keyboard=KeyboardInput(vk=0x12, flags=flags))
        assert u.SendInput(1, C.byref(event), C.sizeof(Input)) == 1
    assert u.SetForegroundWindow(hwnd), "Windows refused fixture activation"
    time.sleep(1)
    client = W.RECT(); u.GetClientRect(hwnd, C.byref(client))
    failures = []
    # 1.0.1 still returned HTCAPTION, but prevented the non-client mouse-down
    # default. Checking hit tests alone would have missed the broken OS drag.
    for name, x, y, should_move in [("brand", 65, 22, True), ("middle", 240, 22, True), ("settings control", 380, 22, False), ("content", 240, 420, False)]:
        before = rect(hwnd)
        point = W.POINT(int(x * client.right / 480), int(y * client.bottom / 660))
        u.ClientToScreen(hwnd, C.byref(point)); u.SetCursorPos(point.x, point.y)
        time.sleep(.2)
        hit = u.SendMessageW(hwnd, 0x0084, 0, (point.x & 0xffff) | ((point.y & 0xffff) << 16))
        assert u.GetForegroundWindow() == hwnd, "Fixture lost foreground ownership"
        mouse_button(0x0002)
        try:
            time.sleep(.15)
            for step in range(1, 9):
                u.SetCursorPos(point.x + step * 10, point.y + step * 5)
                time.sleep(.04)
        finally:
            mouse_button(0x0004)
        time.sleep(.2)
        after = rect(hwnd)
        dx, dy = after.left - before.left, after.top - before.top
        moved = abs(dx) >= 30 and abs(dy) >= 15
        passed = moved if should_move else (dx == 0 and dy == 0)
        if should_move: passed = passed and hit == 2
        print(f"{'PASS' if passed else 'FAIL'}: {name}, hit={hit}, movement=({dx}, {dy})")
        if not passed: failures.append(name)
        u.SetWindowPos(hwnd, None, before.left, before.top, 0, 0, 0x0015)
        time.sleep(.2)
    assert not failures, f"Native window drag regression: {', '.join(failures)}"
finally:
    mouse_button(0x0004)
    process.terminate(); process.wait(timeout=10)
    u.SetCursorPos(cursor.x, cursor.y)
