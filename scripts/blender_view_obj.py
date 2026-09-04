"""Blender startup script: import an OBJ passed after `--`, frame the view.

Usage: blender --python scripts/blender_view_obj.py -- /tmp/small_final.obj
"""

import sys

import bpy

argv = sys.argv[sys.argv.index("--") + 1:] if "--" in sys.argv else []
if not argv:
    raise SystemExit("pass the .obj path after --")
obj_path = argv[0]


def do_import():
    windows = bpy.context.window_manager.windows
    if not windows:
        # Window not created yet at --python startup time; try again shortly.
        return 0.2

    # Clear the default cube/camera/light via the data API rather than the
    # wm.read_factory_settings operator: that operator tears down and
    # recreates the window synchronously, and bpy.context still points at
    # the stale window in the same tick, so the next operator call fails
    # with "poll() failed, context is incorrect".
    for obj in list(bpy.data.objects):
        bpy.data.objects.remove(obj, do_unlink=True)

    bpy.ops.wm.obj_import(filepath=obj_path)

    window = windows[0]
    area = next((a for a in window.screen.areas if a.type == "VIEW_3D"), None)
    if area is not None:
        with bpy.context.temp_override(window=window, area=area):
            bpy.ops.view3d.view_all()
    return None


bpy.app.timers.register(do_import, first_interval=0.1)
