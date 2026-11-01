# dmgbuild settings for the EVB IDE disk image, used by script/evb-release.
# background.png is 600 x 450, so the Finder window is that size, and background.svg draws the
# arrow between the icon positions below. dmgbuild merges background@2x.png into a HiDPI TIFF.
import os.path

application = defines["application"]
application_name = os.path.basename(application)

format = "UDZO"
filesystem = "HFS+"
files = [application]
symlinks = {"Applications": "/Applications"}

background = defines["background"]
window_rect = ((400, 200), (600, 450))
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
icon_size = 128
text_size = 13
icon_locations = {
    application_name: (160, 185),
    "Applications": (440, 185),
}
