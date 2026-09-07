"""Verify disclosure and per-friend mute with native mouse and keyboard input."""
import argparse
import ctypes as C
from ctypes import wintypes as W
import json
import os
from pathlib import Path
import subprocess
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("worktree", type=Path)
p.add_argument("--binary", type=Path)
args = p.parse_args()
root = args.worktree.resolve()
assert (root / "crates/orange-client/src/capture_fixture.rs").is_file(), "Use a disposable fixture"

u = C.WinDLL("user32", use_last_error=True)
u.SetProcessDpiAwarenessContext.argtypes = [W.HANDLE]
u.SetProcessDpiAwarenessContext(C.c_void_p(-4))
u.GetClientRect.argtypes = [W.HWND, C.POINTER(W.RECT)]
u.GetWindowThreadProcessId.argtypes = [W.HWND, C.POINTER(W.DWORD)]
u.IsWindowVisible.argtypes = [W.HWND]
u.SetForegroundWindow.argtypes = [W.HWND]
u.GetForegroundWindow.restype = W.HWND
u.GetCursorPos.argtypes = [C.POINTER(W.POINT)]
u.SetCursorPos.argtypes = [C.c_int, C.c_int]
u.ClientToScreen.argtypes = [W.HWND, C.POINTER(W.POINT)]
callback_type = C.WINFUNCTYPE(W.BOOL, W.HWND, W.LPARAM)
u.EnumWindows.argtypes = [callback_type, W.LPARAM]

class Mouse(C.Structure):
    _fields_ = [("dx", W.LONG), ("dy", W.LONG), ("data", W.DWORD), ("flags", W.DWORD), ("time", W.DWORD), ("extra", W.WPARAM)]
class Keyboard(C.Structure):
    _fields_ = [("vk", W.WORD), ("scan", W.WORD), ("flags", W.DWORD), ("time", W.DWORD), ("extra", W.WPARAM)]
class InputData(C.Union):
    _fields_ = [("mouse", Mouse), ("keyboard", Keyboard)]
class Input(C.Structure):
    _anonymous_ = ("data",)
    _fields_ = [("type", W.DWORD), ("data", InputData)]
u.SendInput.argtypes = [W.UINT, C.POINTER(Input), C.c_int]

def send(event):
    assert u.SendInput(1, C.byref(event), C.sizeof(Input)) == 1

def key(vk):
    send(Input(type=1, keyboard=Keyboard(vk=vk)))
    send(Input(type=1, keyboard=Keyboard(vk=vk, flags=2)))

def click(hwnd, x, y):
    assert u.GetForegroundWindow() == hwnd, "Fixture lost foreground ownership"
    rect = W.RECT(); u.GetClientRect(hwnd, C.byref(rect))
    point = W.POINT(int(x * rect.right / 480), int(y * rect.bottom / 660))
    u.ClientToScreen(hwnd, C.byref(point)); u.SetCursorPos(point.x, point.y)
    send(Input(type=0, mouse=Mouse(flags=2)))
    send(Input(type=0, mouse=Mouse(flags=4)))
    time.sleep(.2)

def preferences():
    time.sleep(.2)
    return json.loads((root / "capture-profile/orange/preferences.json").read_text())

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
        assert process.poll() is None, "Fixture exited before paint"
        u.EnumWindows(find, 0); time.sleep(.1)
    assert found, "No fixture window"
    hwnd = found[0]
    # A background test runner can otherwise hit Windows' foreground lock.
    key(0x12)
    assert u.SetForegroundWindow(hwnd)
    time.sleep(1)
    click(hwnd, 230, 113)
    assert preferences()["friends_panel_collapsed"] is True
    # GPUI already emits clicks for Space/Enter. An extra key handler toggled
    # twice, leaving the panel unchanged; Rust state-only tests missed it.
    key(0x20)
    assert preferences()["friends_panel_collapsed"] is False, "Space toggled twice or did nothing"
    key(0x0D)
    assert preferences()["friends_panel_collapsed"] is True, "Enter toggled twice or did nothing"
    print("PASS: mouse, Space and Enter toggle and persist the panel exactly once")
    click(hwnd, 434, 250)
    key(0x0D)
    account = preferences()["friend_accounts"]["100000000000000001"]
    assert account["muted_stream_alert_friend_ids"] == ["demo-fragbyte"]
    click(hwnd, 434, 250)
    key(0x09); time.sleep(.2)
    key(0x1B); time.sleep(.2)
    click(hwnd, 434, 250)
    key(0x20)
    account = preferences()["friend_accounts"]["100000000000000001"]
    assert account["muted_stream_alert_friend_ids"] == []
    assert [f["id"] for f in account["friends"]] == ["demo-fragbyte", "demo-nightshift"]
    print("PASS: Enter mutes, Tab/Escape dismisses, Space unmutes the same friend")
finally:
    process.terminate(); process.wait(timeout=10)
    u.SetCursorPos(cursor.x, cursor.y)
