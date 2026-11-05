#!/usr/bin/env python3
"""Clicks and types on a private Xvfb display through XTEST.
usage: xclick.py :NUM click X Y | type TEXT | key KEYSYM_NAME | move X Y
"""
import ctypes, sys, time
X = ctypes.CDLL("libX11.so.6"); T = ctypes.CDLL("libXtst.so.6")
X.XOpenDisplay.restype = ctypes.c_void_p
X.XOpenDisplay.argtypes = [ctypes.c_char_p]
for f in (X.XFlush, X.XCloseDisplay):
    f.argtypes = [ctypes.c_void_p]
X.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
X.XStringToKeysym.restype = ctypes.c_ulong
X.XStringToKeysym.argtypes = [ctypes.c_char_p]
T.XTestFakeMotionEvent.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_ulong]
T.XTestFakeButtonEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
T.XTestFakeKeyEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
d = X.XOpenDisplay(sys.argv[1].encode())
assert d, "no display"
cmd = sys.argv[2]
def move(x, y):
    T.XTestFakeMotionEvent(d, -1, x, y, 0); X.XFlush(d); time.sleep(0.12)
def focus():
    # Xvfb has no window manager, so nothing assigns keyboard focus by itself.
    X.XDefaultRootWindow.restype = ctypes.c_ulong
    X.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    X.XQueryTree.argtypes = [ctypes.c_void_p, ctypes.c_ulong] + [ctypes.c_void_p] * 4
    X.XSetInputFocus.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
    r, p, kids, n = ctypes.c_ulong(), ctypes.c_ulong(), ctypes.POINTER(ctypes.c_ulong)(), ctypes.c_uint()
    X.XQueryTree(d, X.XDefaultRootWindow(d), ctypes.byref(r), ctypes.byref(p), ctypes.byref(kids), ctypes.byref(n))
    if n.value:
        X.XSetInputFocus(d, kids[n.value - 1], 1, 0)
        X.XFlush(d); time.sleep(0.1)
def resize(w, h):
    focus()
    X.XMoveResizeWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int, ctypes.c_uint, ctypes.c_uint]
    X.XDefaultRootWindow.restype = ctypes.c_ulong
    X.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    r, p, kids, n = ctypes.c_ulong(), ctypes.c_ulong(), ctypes.POINTER(ctypes.c_ulong)(), ctypes.c_uint()
    X.XQueryTree(d, X.XDefaultRootWindow(d), ctypes.byref(r), ctypes.byref(p), ctypes.byref(kids), ctypes.byref(n))
    X.XMoveResizeWindow(d, kids[n.value - 1], 0, 0, w, h)
    X.XFlush(d); time.sleep(0.2)
def key(name):
    code = X.XKeysymToKeycode(d, X.XStringToKeysym(name.encode()))
    T.XTestFakeKeyEvent(d, code, 1, 0); T.XTestFakeKeyEvent(d, code, 0, 0); X.XFlush(d); time.sleep(0.05)
if cmd == "click":
    move(int(sys.argv[3]), int(sys.argv[4]))
    T.XTestFakeButtonEvent(d, 1, 1, 0); X.XFlush(d); time.sleep(0.06)
    T.XTestFakeButtonEvent(d, 1, 0, 0); X.XFlush(d)
elif cmd == "move":
    move(int(sys.argv[3]), int(sys.argv[4]))
elif cmd == "type":
    focus()
    for ch in sys.argv[3]:
        key({" ": "space", "-": "minus"}.get(ch, ch))
elif cmd == "resize":
    resize(int(sys.argv[3]), int(sys.argv[4]))
elif cmd == "key":
    focus()
    key(sys.argv[3])
X.XCloseDisplay(d)
