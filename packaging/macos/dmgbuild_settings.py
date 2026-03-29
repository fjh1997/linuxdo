import os


def _define(name, default=None):
    return defines.get(name, default)


app_path = os.path.abspath(_define("app"))
app_name = os.path.basename(app_path)
volume_name = _define("volume_name", app_name[:-4] if app_name.endswith(".app") else app_name)
background = os.path.abspath(_define("background", "assets/dmg/background.png"))
window_width = int(_define("window_width", "720"))
window_height = int(_define("window_height", "460"))
app_x = int(_define("app_x", "160"))
app_y = int(_define("app_y", "220"))
apps_x = int(_define("apps_x", "560"))
apps_y = int(_define("apps_y", "220"))


format = "UDZO"
files = [app_path]
symlinks = {"Applications": "/Applications"}
hide_extensions = [app_name]

default_view = "icon-view"
show_toolbar = False
show_sidebar = False
show_status_bar = False
show_tab_view = False
show_pathbar = False
show_icon_preview = False
include_icon_view_settings = True

background = background
window_rect = ((120, 120), (window_width, window_height))

arrange_by = None
grid_spacing = 100
label_pos = "bottom"
text_size = 16
icon_size = 128

icon_locations = {
    app_name: (app_x, app_y),
    "Applications": (apps_x, apps_y),
}
