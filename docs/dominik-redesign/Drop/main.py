import gi

gi.require_version("Adw", "1")
gi.require_version("Gtk", "4.0")
from gi.repository import Gio
import workbench

dialog = workbench.builder.get_object("dialog")
button = workbench.builder.get_object("testbutton")
image = workbench.builder.get_object("image")


def on_button_clicked(_button):
    dialog.present(workbench.window)


def on_dialog_close_attempt(_dialog):
    print("Close Attempt")
    dialog.force_close()


def on_dialog_closed(_dialog):
    print("Closed")


button.connect("clicked", on_button_clicked)
dialog.connect("close-attempt", on_dialog_close_attempt)
dialog.connect("closed", on_dialog_closed)

dialog = workbench.builder.get_object("dialog")
button = workbench.builder.get_object("continue")


def on_button_clicked(_button):
    dialog.force_close()
