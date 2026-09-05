"""Capture only the fixture process's GPUI client area with Win32 PrintWindow."""
import argparse
import ctypes as C
from ctypes import wintypes as W
import os
from pathlib import Path
import subprocess
import time
from PIL import Image

p=argparse.ArgumentParser()
p.add_argument("worktree",type=Path)
p.add_argument("--output",type=Path,default=Path(__file__).resolve().parent.parent/"screenshots")
args=p.parse_args()
u=C.WinDLL("user32",use_last_error=True)
g=C.WinDLL("gdi32",use_last_error=True)
u.SetProcessDpiAwarenessContext.argtypes=[W.HANDLE]
u.SetProcessDpiAwarenessContext(C.c_void_p(-4))
u.GetDC.argtypes=[W.HWND];u.GetDC.restype=W.HDC
u.ReleaseDC.argtypes=[W.HWND,W.HDC]
u.GetClientRect.argtypes=[W.HWND,C.POINTER(W.RECT)]
u.GetWindowThreadProcessId.argtypes=[W.HWND,C.POINTER(W.DWORD)]
u.IsWindowVisible.argtypes=[W.HWND];u.IsWindowVisible.restype=W.BOOL
u.SetForegroundWindow.argtypes=[W.HWND]
u.SetCursorPos.argtypes=[C.c_int,C.c_int]
u.GetCursorPos.argtypes=[C.POINTER(W.POINT)]
u.ClientToScreen.argtypes=[W.HWND,C.POINTER(W.POINT)]
u.PrintWindow.argtypes=[W.HWND,W.HDC,W.UINT];u.PrintWindow.restype=W.BOOL
g.CreateCompatibleDC.argtypes=[W.HDC];g.CreateCompatibleDC.restype=W.HDC
g.CreateCompatibleBitmap.argtypes=[W.HDC,C.c_int,C.c_int];g.CreateCompatibleBitmap.restype=W.HBITMAP
g.SelectObject.argtypes=[W.HDC,W.HANDLE];g.SelectObject.restype=W.HANDLE
g.DeleteObject.argtypes=[W.HANDLE]
g.DeleteDC.argtypes=[W.HDC]
class BITMAPINFOHEADER(C.Structure):
    _fields_=[("biSize",W.DWORD),("biWidth",W.LONG),("biHeight",W.LONG),("biPlanes",W.WORD),("biBitCount",W.WORD),("biCompression",W.DWORD),("biSizeImage",W.DWORD),("biXPelsPerMeter",W.LONG),("biYPelsPerMeter",W.LONG),("biClrUsed",W.DWORD),("biClrImportant",W.DWORD)]
g.GetDIBits.argtypes=[W.HDC,W.HBITMAP,W.UINT,W.UINT,W.LPVOID,C.POINTER(BITMAPINFOHEADER),W.UINT]
g.GetDIBits.restype=C.c_int
callback_type=C.WINFUNCTYPE(W.BOOL,W.HWND,W.LPARAM)
u.EnumWindows.argtypes=[callback_type,W.LPARAM]

def find_window(pid):
    found=[]
    @callback_type
    def callback(hwnd,_):
        process=W.DWORD()
        u.GetWindowThreadProcessId(hwnd,C.byref(process))
        if process.value==pid and u.IsWindowVisible(hwnd): found.append(hwnd)
        return True
    u.EnumWindows(callback,0)
    return found[0] if found else None

def capture(hwnd,path):
    rect=W.RECT();assert u.GetClientRect(hwnd,C.byref(rect))
    w,h=rect.right,rect.bottom
    dc=u.GetDC(hwnd);memory=g.CreateCompatibleDC(dc)
    bitmap=g.CreateCompatibleBitmap(dc,w,h);old=g.SelectObject(memory,bitmap)
    try:
        assert u.PrintWindow(hwnd,memory,3), "PrintWindow failed"
        g.SelectObject(memory,old)
        info=BITMAPINFOHEADER(C.sizeof(BITMAPINFOHEADER),w,-h,1,32,0,w*h*4,0,0,0,0)
        data=C.create_string_buffer(w*h*4)
        assert g.GetDIBits(memory,bitmap,0,h,data,C.byref(info),0)==h
        image=Image.frombuffer("RGB",(w,h),data,"raw","BGRX",0,1)
        assert len(image.getcolors(w*h) or [])>64, "Capture appears blank"
        image.save(path,optimize=True)
        print(f"{path}: {w} x {h}, {path.stat().st_size:,} bytes")
    finally:
        g.DeleteObject(bitmap);g.DeleteDC(memory);u.ReleaseDC(hwnd,dc)

args.output.mkdir(parents=True,exist_ok=True)
env=os.environ.copy()
env["ORANGE_CAPTURE_ASSETS"]=str(args.worktree.resolve()/"capture-art")
env["APPDATA"]=str(args.worktree.resolve()/"capture-profile")
env["LOCALAPPDATA"]=str(args.worktree.resolve()/"capture-local")
env["ORANGE_SERVER"]="ws://127.0.0.1:9/ws"
original_cursor=W.POINT()
u.GetCursorPos(C.byref(original_cursor))
for screen in ["home","pick","streaming"]:
    env["ORANGE_CAPTURE_SCREEN"]=screen
    process=subprocess.Popen([str(args.worktree.resolve()/"target/debug/orange-tray.exe")],env=env,cwd=args.worktree)
    try:
        hwnd=None
        for _ in range(100):
            if process.poll() is not None:raise RuntimeError(f"Fixture exited: {process.returncode}")
            hwnd=find_window(process.pid)
            if hwnd:break
            time.sleep(.1)
        assert hwnd,"No native fixture window"
        u.SetForegroundWindow(hwnd)
        if screen == "pick":
            # Hover the source shown in the next workflow screenshot.
            rect=W.RECT();u.GetClientRect(hwnd,C.byref(rect))
            point=W.POINT(int(150*rect.right/480),int(290*rect.bottom/660))
            u.ClientToScreen(hwnd,C.byref(point))
            u.SetCursorPos(point.x,point.y)
        else:
            u.SetCursorPos(0,0)
        time.sleep(3)
        capture(hwnd,args.output/f"{screen}.png")
    finally:
        process.terminate()
        process.wait(timeout=10)
        u.SetCursorPos(original_cursor.x,original_cursor.y)
