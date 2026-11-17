#!/usr/bin/env python3

import os

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import GLib, Gtk

SUBSCRIBED_MARKER = "/tmp/e2e-subscribed"
READY_MARKER = "/tmp/e2e-ready"
TRIGGER_MARKER = "/tmp/e2e-do-edit"
DONE_MARKER = "/tmp/e2e-edit-done"


class TestApp(Gtk.Application):
    def __init__(self):
        super().__init__(application_id="com.test.CorrectionE2E")
        self._entry = None
        self._edited = False

    def do_activate(self):
        self.hold()
        with open("/tmp/test-app.pid", "w") as f:
            f.write(str(os.getpid()))
        GLib.timeout_add(100, self._poll_subscribed)

    def _poll_subscribed(self):
        if not os.path.exists(SUBSCRIBED_MARKER):
            return True

        window = Gtk.ApplicationWindow(application=self, title="Test Window")
        entry = Gtk.Entry()
        entry.set_text("the cash is here")
        window.set_child(entry)
        window.present()
        self._entry = entry
        self.release()

        with open(READY_MARKER, "w") as f:
            f.write("ready\n")

        GLib.timeout_add(100, self._poll_trigger)
        return False

    def _poll_trigger(self):
        if self._edited:
            return True
        if not os.path.exists(TRIGGER_MARKER):
            return True

        self._edited = True

        editable = self._entry
        editable.delete_text(4, 8)
        editable.insert_text("cache", 4)

        with open(DONE_MARKER, "w") as f:
            f.write("done\n")

        return True


if __name__ == "__main__":
    app = TestApp()
    app.run(None)
