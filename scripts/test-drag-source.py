#!/usr/bin/env python3
"""GTK3 file-manager-like source for the isolated GTK4 drop regression (Xvfb)."""
import ctypes as C
import ctypes.util
import sys
import gi

gi.require_version('Gtk', '3.0')
from gi.repository import Gtk, Gdk, GLib

x = C.CDLL(ctypes.util.find_library('X11'))
xt = C.CDLL(ctypes.util.find_library('Xtst'))
x.XOpenDisplay.restype = C.c_void_p
x.XDefaultRootWindow.argtypes = [C.c_void_p]
x.XDefaultRootWindow.restype = C.c_ulong
x.XQueryTree.argtypes = [C.c_void_p, C.c_ulong, C.POINTER(C.c_ulong), C.POINTER(C.c_ulong), C.POINTER(C.POINTER(C.c_ulong)), C.POINTER(C.c_uint)]
x.XFetchName.argtypes = [C.c_void_p, C.c_ulong, C.POINTER(C.c_char_p)]
x.XFree.argtypes = [C.c_void_p]
x.XMoveResizeWindow.argtypes = [C.c_void_p, C.c_ulong, C.c_int, C.c_int, C.c_uint, C.c_uint]
x.XFlush.argtypes = [C.c_void_p]
xt.XTestFakeMotionEvent.argtypes = [C.c_void_p, C.c_int, C.c_int, C.c_int, C.c_ulong]
xt.XTestFakeButtonEvent.argtypes = [C.c_void_p, C.c_uint, C.c_int, C.c_ulong]
display = x.XOpenDisplay(None)
assert display


def find(title):
    root = x.XDefaultRootWindow(display)
    parent, actual_root, count = C.c_ulong(), C.c_ulong(), C.c_uint()
    children = C.POINTER(C.c_ulong)()
    assert x.XQueryTree(display, root, C.byref(actual_root), C.byref(parent), C.byref(children), C.byref(count))
    found = None
    for index in range(count.value):
        name = C.c_char_p()
        if x.XFetchName(display, children[index], C.byref(name)) and name.value:
            if name.value.decode(errors='replace') == title:
                found = children[index]
            x.XFree(name)
    x.XFree(children)
    return found


window = Gtk.Window(title='Packet Lab drag source')
window.set_default_size(250, 200)
box = Gtk.EventBox()
box.add(Gtk.Label(label='Drag these test files'))
window.add(box)
box.drag_source_set(Gdk.ModifierType.BUTTON1_MASK, [
    Gtk.TargetEntry.new('application/vnd.portal.filetransfer', 0, 1),
    Gtk.TargetEntry.new('text/uri-list', 0, 0),
], Gdk.DragAction.COPY)
requested = []


def provide(_widget, _context, selection, info, _time):
    requested.append(info)
    if info == 0:
        selection.set_uris(sys.argv[1:])
    else:
        selection.set(selection.get_target(), 8, b'invalid-transfer')


box.connect('drag-data-get', provide)
box.connect('button-press-event', lambda *_: (print('GTK3 source button pressed', flush=True), False)[1])
box.connect('drag-begin', lambda *_: print('GTK3 drag began', flush=True))
box.connect('drag-failed', lambda _, context, reason: print('GTK3 drag failed:', reason, flush=True))
box.connect('drag-end', lambda *_: Gtk.main_quit())
window.show_all()


def move(px, py):
    xt.XTestFakeMotionEvent(display, -1, px, py, 0)
    x.XFlush(display)


def release():
    xt.XTestFakeButtonEvent(display, 1, 0, 0)
    x.XFlush(display)
    return False


def begin():
    target, source = find('Packet Lab drop test'), find('Packet Lab drag source')
    assert target and source
    x.XMoveResizeWindow(display, target, 400, 0, 700, 600)
    x.XMoveResizeWindow(display, source, 0, 0, 250, 200)
    GLib.timeout_add(200, start_drag)
    return False


def start_drag():
    move(100, 100)
    xt.XTestFakeButtonEvent(display, 1, 1, 0)
    x.XFlush(display)
    for step in range(1, 21):
        GLib.timeout_add(step * 30, lambda step=step: (move(100 + step * 30, 100 + step * 8), False)[1])
    GLib.timeout_add(1000, release)
    return False


GLib.timeout_add(300, begin)
GLib.timeout_add(6000, lambda: (Gtk.main_quit(), False)[1])
Gtk.main()
assert requested == [0], f'Expected raw URI request, got {requested}'
print('Cross-process GTK3 -> GTK4 drop used raw URIs, not the invalid portal token')
