#!/usr/bin/env python3
"""Scriptable mock of the XDG desktop portal frontend for Footy protocol tests.

Used by run.sh (the ignored `mock_portal` Rust tests) and by the Flatpak
session harness. It only ever runs on a private session bus.

Implements GlobalShortcuts v2, RemoteDesktop v2, Clipboard v1, Background,
Notification and a StatusNotifierWatcher on a private session bus. Every call
is appended to a JSON log; org.footy.Mock drives signals from the test.

Environment:
  MOCK_DENY_BIND=1      BindShortcuts responds 1 (cancelled)
  MOCK_START_CODE=<n>   RemoteDesktop.Start response code (default 0)
  MOCK_NO_CLIPBOARD=1   Start reports clipboard_enabled=false
  MOCK_SNI_ONLY=1       only provide the tray watcher (real portal in use)
"""
import json
import os
import sys
import threading
import time

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

DESKTOP = "/org/freedesktop/portal/desktop"
LOG = []
STATE = {
    "gs_session": None,
    "gs_sender": None,
    "rd_session": None,
    "rd_sender": None,
    "serial": 100,
    "pending_reads": {},
    "pressed": set(),
    "selection_mimes": [],
    "foreign": None,  # (mimes, text)
    "start_count": 0,
    "pasted": [],
    "readers": {},
    "read_delay_ms": 0,
    "footy_owns": False,
    "deny_bind": os.environ.get("MOCK_DENY_BIND") == "1",
    "start_code": int(os.environ.get("MOCK_START_CODE", "0")),
}

XML = """
<node>
 <interface name="org.freedesktop.portal.GlobalShortcuts">
  <method name="CreateSession"><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
  <method name="BindShortcuts"><arg type="o" direction="in"/><arg type="a(sa{sv})" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
  <method name="ListShortcuts"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
  <method name="ConfigureShortcuts"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/></method>
  <signal name="Activated"><arg type="o"/><arg type="s"/><arg type="t"/><arg type="a{sv}"/></signal>
  <signal name="Deactivated"><arg type="o"/><arg type="s"/><arg type="t"/><arg type="a{sv}"/></signal>
  <signal name="ShortcutsChanged"><arg type="o"/><arg type="a(sa{sv})"/></signal>
  <property name="version" type="u" access="read"/>
 </interface>
 <interface name="org.freedesktop.portal.RemoteDesktop">
  <method name="CreateSession"><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
  <method name="SelectDevices"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
  <method name="Start"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
  <method name="NotifyKeyboardKeysym"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="in"/></method>
  <method name="NotifyKeyboardKeycode"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="in"/></method>
  <property name="AvailableDeviceTypes" type="u" access="read"/>
  <property name="version" type="u" access="read"/>
 </interface>
 <interface name="org.freedesktop.portal.Clipboard">
  <method name="RequestClipboard"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/></method>
  <method name="SetSelection"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/></method>
  <method name="SelectionWrite"><arg type="o" direction="in"/><arg type="u" direction="in"/><arg type="h" direction="out"/></method>
  <method name="SelectionWriteDone"><arg type="o" direction="in"/><arg type="u" direction="in"/><arg type="b" direction="in"/></method>
  <method name="SelectionRead"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="h" direction="out"/></method>
  <signal name="SelectionOwnerChanged"><arg type="o"/><arg type="a{sv}"/></signal>
  <signal name="SelectionTransfer"><arg type="o"/><arg type="s"/><arg type="u"/></signal>
  <property name="version" type="u" access="read"/>
 </interface>
 <interface name="org.freedesktop.portal.Background">
  <method name="RequestBackground"><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
  <method name="SetStatus"><arg type="a{sv}" direction="in"/></method>
  <property name="version" type="u" access="read"/>
 </interface>
 <interface name="org.freedesktop.portal.Notification">
  <method name="AddNotification"><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/></method>
  <method name="RemoveNotification"><arg type="s" direction="in"/></method>
  <signal name="ActionInvoked"><arg type="s"/><arg type="s"/><arg type="av"/></signal>
  <property name="version" type="u" access="read"/>
  <property name="SupportedOptions" type="a{sv}" access="read"/>
 </interface>
</node>
"""

SESSION_XML = """
<node>
 <interface name="org.freedesktop.portal.Session">
  <method name="Close"/>
  <signal name="Closed"><arg type="a{sv}"/></signal>
  <property name="version" type="u" access="read"/>
 </interface>
</node>
"""

REQUEST_XML = """
<node>
 <interface name="org.freedesktop.portal.Request">
  <method name="Close"/>
  <signal name="Response"><arg type="u"/><arg type="a{sv}"/></signal>
 </interface>
</node>
"""

MOCK_XML = """
<node>
 <interface name="org.footy.Mock">
  <method name="Log"><arg type="s" direction="out"/></method>
  <method name="EmitShortcut"><arg type="s" direction="in" name="kind"/><arg type="s" direction="in" name="which"/><arg type="s" direction="in" name="id"/></method>
  <method name="CloseSession"><arg type="s" direction="in" name="which"/></method>
  <method name="SetForeignSelection"><arg type="as" direction="in"/><arg type="s" direction="in"/></method>
  <method name="RequestTransfer"><arg type="s" direction="in"/></method>
  <method name="SetReadDelay"><arg type="u" direction="in"/></method>
  <method name="SetDenyBind"><arg type="b" direction="in"/></method>
  <method name="SetStartCode"><arg type="u" direction="in"/></method>
 </interface>
 <interface name="org.kde.StatusNotifierWatcher">
  <method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
  <method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
  <signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
  <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
  <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
  <property name="ProtocolVersion" type="i" access="read"/>
 </interface>
</node>
"""

conn = None
items = []


def log(event, **data):
    entry = {"t": round(time.monotonic(), 3), "event": event, **data}
    LOG.append(entry)
    print(json.dumps(entry), flush=True)


def plain(value):
    if isinstance(value, GLib.Variant):
        return plain(value.unpack())
    if isinstance(value, dict):
        return {k: plain(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [plain(v) for v in value]
    return value


def sender_part(sender):
    return sender[1:].replace(".", "_")


def register(path, xml, handler, props=None):
    info = Gio.DBusNodeInfo.new_for_xml(xml)
    for iface in info.interfaces:
        conn.register_object(
            path,
            iface,
            handler,
            (lambda c, s, p, i, name: GLib.Variant(*props(i, name)))
            if props
            else None,
            None,
        )


def request_object(sender, options):
    token = plain(options).get("handle_token", "t%d" % time.time_ns())
    path = "/org/freedesktop/portal/desktop/request/%s/%s" % (sender_part(sender), token)
    try:
        register(path, REQUEST_XML, lambda *a: a[-1].return_value(None))
    except GLib.Error:
        pass
    return path


def respond(path, sender, code, results, delay=0.05):
    def emit():
        conn.emit_signal(
            sender,
            path,
            "org.freedesktop.portal.Request",
            "Response",
            GLib.Variant("(ua{sv})", (code, results)),
        )
        return False

    GLib.timeout_add(int(delay * 1000), emit)


def session_object(sender, options, kind):
    token = plain(options)["session_handle_token"]
    path = "/org/freedesktop/portal/desktop/session/%s/%s" % (sender_part(sender), token)

    def handler(c, s, p, iface, method, params, inv):
        log("Session.Close", kind=kind, path=p)
        inv.return_value(None)
        if STATE[kind + "_session"] == p:
            STATE[kind + "_session"] = None

    register(path, SESSION_XML, handler, lambda i, n: ("u", 1))
    log("SessionCreated", kind=kind, path=path)
    STATE[kind + "_session"] = path
    STATE[kind + "_sender"] = sender
    return path


def shortcuts_reply(shortcuts):
    out = []
    for sid, info in shortcuts:
        info = plain(info)
        trigger = info.get("preferred_trigger") or ""
        out.append(
            (
                sid,
                {
                    "description": GLib.Variant("s", info.get("description", "")),
                    "trigger_description": GLib.Variant("s", trigger.replace("CTRL", "Ctrl")),
                },
            )
        )
    return out


def write_pipe(data):
    r, w = os.pipe()

    def worker():
        with os.fdopen(w, "wb") as f:
            f.write(data)

    threading.Thread(target=worker, daemon=True).start()
    return r


def desktop_method(c, sender, path, iface, method, params, inv):
    args = params.unpack()
    short = iface.rsplit(".", 1)[-1]
    if not method.startswith("NotifyKeyboard"):
        log(short + "." + method, args=plain(args))

    if iface.endswith("GlobalShortcuts"):
        if method == "CreateSession":
            req = request_object(sender, args[0])
            sess = session_object(sender, args[0], "gs")
            inv.return_value(GLib.Variant("(o)", (req,)))
            respond(req, sender, 0, {"session_handle": GLib.Variant("s", sess)})
        elif method == "BindShortcuts":
            req = request_object(sender, args[3])
            inv.return_value(GLib.Variant("(o)", (req,)))
            if STATE["deny_bind"]:
                respond(req, sender, 1, {})
            else:
                respond(
                    req,
                    sender,
                    0,
                    {"shortcuts": GLib.Variant("a(sa{sv})", shortcuts_reply(args[1]))},
                )
        elif method == "ConfigureShortcuts":
            inv.return_value(None)
        else:
            req = request_object(sender, args[-1])
            inv.return_value(GLib.Variant("(o)", (req,)))
            respond(req, sender, 0, {"shortcuts": GLib.Variant("a(sa{sv})", [])})
        return

    if iface.endswith("RemoteDesktop"):
        if method == "CreateSession":
            req = request_object(sender, args[0])
            sess = session_object(sender, args[0], "rd")
            inv.return_value(GLib.Variant("(o)", (req,)))
            respond(req, sender, 0, {"session_handle": GLib.Variant("s", sess)})
        elif method == "SelectDevices":
            req = request_object(sender, args[1])
            inv.return_value(GLib.Variant("(o)", (req,)))
            respond(req, sender, 0, {})
        elif method == "Start":
            req = request_object(sender, args[2])
            inv.return_value(GLib.Variant("(o)", (req,)))
            code = STATE["start_code"]
            STATE["start_count"] += 1
            results = {
                "devices": GLib.Variant("u", 1),
                "clipboard_enabled": GLib.Variant(
                    "b", os.environ.get("MOCK_NO_CLIPBOARD") != "1"
                ),
                "restore_token": GLib.Variant("s", "tok-%d" % STATE["start_count"]),
            }
            respond(req, sender, code, results if code == 0 else {})
        elif method.startswith("NotifyKeyboard"):
            keysym, state = args[2], args[3]
            log("RemoteDesktop." + method, keysym=hex(keysym), state=state)
            inv.return_value(None)
            if state == 1:
                STATE["pressed"].add(keysym)
            else:
                # Simulate the focused app: Ctrl+V release requests the text.
                if keysym == 0x76 and 0xFFE3 in STATE["pressed"]:
                    if STATE["footy_owns"]:
                        GLib.timeout_add(30, lambda: (transfer("text/plain;charset=utf-8"), False)[1])
                    else:
                        log("PastedForeign", text=(STATE["foreign"] or ([], ""))[1])
                STATE["pressed"].discard(keysym)
        return

    if iface.endswith("Clipboard"):
        if method == "SelectionWrite":
            serial = args[1]
            r, w = os.pipe()
            delay = STATE["read_delay_ms"] / 1000.0
            result = {}

            def reader(fd=r, result=result, delay=delay):
                # A target that reads concurrently, optionally slowly.
                time.sleep(delay)
                chunks = []
                with os.fdopen(fd, "rb") as f:
                    while True:
                        chunk = f.read(16384)
                        if not chunk:
                            break
                        chunks.append(chunk)
                        if delay:
                            time.sleep(0.01)
                result["data"] = b"".join(chunks)

            thread = threading.Thread(target=reader, daemon=True)
            thread.start()
            STATE["readers"][serial] = (thread, result)
            fds = Gio.UnixFDList.new()
            idx = fds.append(w)
            os.close(w)
            inv.return_value_with_unix_fd_list(GLib.Variant("(h)", (idx,)), fds)
        elif method == "SelectionWriteDone":
            serial, ok = args[1], args[2]
            inv.return_value(None)
            thread, result = STATE["readers"].pop(serial, (None, {}))
            if thread is not None:
                thread.join(15)
            data = result.get("data")
            text = data.decode("utf-8", "replace") if data is not None else None
            if text is not None and len(text) > 200:
                log("PastedText", serial=serial, ok=ok, text=text[:20], length=len(text), tail=text[-8:])
            else:
                log("PastedText", serial=serial, ok=ok, text=text)
            STATE["pasted"].append(text)
        elif method == "SetSelection":
            STATE["selection_mimes"] = plain(args[1]).get("mime_types", [])
            STATE["footy_owns"] = True
            inv.return_value(None)
            conn.emit_signal(
                STATE["rd_sender"],
                DESKTOP,
                "org.freedesktop.portal.Clipboard",
                "SelectionOwnerChanged",
                GLib.Variant(
                    "(oa{sv})",
                    (
                        args[0],
                        {
                            "mime_types": GLib.Variant("as", STATE["selection_mimes"]),
                            "session_is_owner": GLib.Variant("b", True),
                        },
                    ),
                ),
            )
        elif method == "SelectionRead":
            mimes, text = STATE["foreign"] or ([], "")
            if args[1] not in mimes:
                inv.return_dbus_error("org.freedesktop.portal.Error.Failed", "no such mime")
                return
            fds = Gio.UnixFDList.new()
            r = write_pipe(text.encode())
            idx = fds.append(r)
            os.close(r)
            inv.return_value_with_unix_fd_list(GLib.Variant("(h)", (idx,)), fds)
        else:
            inv.return_value(None)
        return

    if iface.endswith("Background"):
        req = request_object(sender, args[1])
        inv.return_value(GLib.Variant("(o)", (req,)))
        opts = plain(args[1])
        respond(
            req,
            sender,
            0,
            {
                "background": GLib.Variant("b", True),
                "autostart": GLib.Variant("b", bool(opts.get("autostart"))),
            },
        )
        return

    inv.return_value(None)


def transfer(mime):
    STATE["serial"] += 1
    serial = STATE["serial"]
    log("SelectionTransfer", mime=mime, serial=serial)
    conn.emit_signal(
        STATE["rd_sender"],
        DESKTOP,
        "org.freedesktop.portal.Clipboard",
        "SelectionTransfer",
        GLib.Variant("(osu)", (STATE["rd_session"], mime, serial)),
    )


def desktop_props(iface, name):
    if name == "AvailableDeviceTypes":
        return ("u", 7)
    if name == "SupportedOptions":
        return ("a{sv}", {})
    if iface.endswith("GlobalShortcuts") or iface.endswith("RemoteDesktop"):
        return ("u", 2)
    return ("u", 1)


def mock_method(c, sender, path, iface, method, params, inv):
    args = params.unpack()
    if iface == "org.kde.StatusNotifierWatcher":
        log("SNI." + method, args=plain(args), sender=sender)
        if method == "RegisterStatusNotifierItem":
            items.append((sender, args[0]))
        inv.return_value(None)
        return
    if method == "Log":
        inv.return_value(GLib.Variant("(s)", (json.dumps(LOG),)))
        return
    log("Mock." + method, args=plain(args))
    if method == "EmitShortcut":
        kind, which, sid = args
        if which.startswith("/"):
            session = which
        elif which == "ours":
            session = STATE["gs_session"]
        else:
            session = "/org/freedesktop/portal/desktop/session/1_999/foreign"
        conn.emit_signal(
            STATE["gs_sender"],
            DESKTOP,
            "org.freedesktop.portal.GlobalShortcuts",
            kind,
            GLib.Variant("(osta{sv})", (session, sid, int(time.time() * 1000), {})),
        )
    elif method == "CloseSession":
        which = args[0]
        path_ = STATE[which + "_session"]
        if path_:
            conn.emit_signal(
                STATE[which + "_sender"],
                path_,
                "org.freedesktop.portal.Session",
                "Closed",
                GLib.Variant("(a{sv})", ({},)),
            )
            STATE[which + "_session"] = None
    elif method == "SetForeignSelection":
        mimes, text = args
        STATE["foreign"] = (mimes, text)
        STATE["footy_owns"] = False
        conn.emit_signal(
            STATE["rd_sender"],
            DESKTOP,
            "org.freedesktop.portal.Clipboard",
            "SelectionOwnerChanged",
            GLib.Variant(
                "(oa{sv})",
                (
                    STATE["rd_session"],
                    {
                        "mime_types": GLib.Variant("as", mimes),
                        "session_is_owner": GLib.Variant("b", False),
                    },
                ),
            ),
        )
    elif method == "RequestTransfer":
        transfer(args[0])
    elif method == "SetReadDelay":
        STATE["read_delay_ms"] = args[0]
    elif method == "SetDenyBind":
        STATE["deny_bind"] = args[0]
    elif method == "SetStartCode":
        STATE["start_code"] = args[0]
    inv.return_value(None)


def watcher_props(iface, name):
    if name == "RegisteredStatusNotifierItems":
        return ("as", [s + p if p.startswith("/") else p for s, p in items])
    if name == "IsStatusNotifierHostRegistered":
        return ("b", True)
    return ("i", 0)


def on_bus(c, name):
    global conn
    conn = c
    register(DESKTOP, XML, desktop_method, desktop_props)
    register("/org/footy/Mock", MOCK_XML, mock_method, watcher_props)
    register("/StatusNotifierWatcher", MOCK_XML, mock_method, watcher_props)


def on_name(c, name):
    log("NameAcquired", name=name)


def main():
    loop = GLib.MainLoop()
    portal_name = (
        "org.footy.MockPortalUnused"
        if os.environ.get("MOCK_SNI_ONLY") == "1"
        else "org.freedesktop.portal.Desktop"
    )
    Gio.bus_own_name(
        Gio.BusType.SESSION,
        portal_name,
        Gio.BusNameOwnerFlags.NONE,
        on_bus,
        on_name,
        lambda c, n: (log("NameLost", name=n), loop.quit()),
    )
    for extra in ("org.footy.Mock", "org.kde.StatusNotifierWatcher"):
        Gio.bus_own_name(
            Gio.BusType.SESSION, extra, Gio.BusNameOwnerFlags.NONE, None, on_name, None
        )
    loop.run()


if __name__ == "__main__":
    sys.exit(main())
