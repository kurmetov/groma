"""Headless Blender render of an imported OBJ, for visual inspection without
a GUI session. Frames all mesh objects and renders one PNG.

Usage: blender --background --python scripts/blender_render_obj.py -- \
    /tmp/small_final.obj /tmp/small_final.png [distance_scale]
"""

import math
import sys

import bpy
import mathutils

argv = sys.argv[sys.argv.index("--") + 1 :] if "--" in sys.argv else []
if len(argv) < 2:
    raise SystemExit("usage: ... -- <input.obj> <output.png> [distance_scale] [name_filter]")
obj_path, png_path = argv[0], argv[1]
distance_scale = float(argv[2]) if len(argv) > 2 else 1.8
name_filter = argv[3] if len(argv) > 3 else None

bpy.ops.wm.read_factory_settings(use_empty=True)
bpy.ops.wm.obj_import(filepath=obj_path)

meshes = [obj for obj in bpy.context.scene.objects if obj.type == "MESH"]
if name_filter:
    keep = [obj for obj in meshes if name_filter in obj.name]
    for obj in meshes:
        if obj not in keep:
            bpy.data.objects.remove(obj, do_unlink=True)
    meshes = keep
if not meshes:
    raise SystemExit("no mesh objects imported (check the name filter)")
print(f"Keeping {len(meshes)} objects: {[m.name for m in meshes]}")

minimum = mathutils.Vector((math.inf, math.inf, math.inf))
maximum = mathutils.Vector((-math.inf, -math.inf, -math.inf))
for obj in meshes:
    for corner in obj.bound_box:
        world = obj.matrix_world @ mathutils.Vector(corner)
        minimum = mathutils.Vector(min(a, b) for a, b in zip(minimum, world))
        maximum = mathutils.Vector(max(a, b) for a, b in zip(maximum, world))

center = (minimum + maximum) / 2
extent = maximum - minimum
radius = max(extent.length / 2, 0.1)
distance = radius * distance_scale

direction = mathutils.Vector((1, -1, 0.7)).normalized()
camera_location = center + direction * distance

camera_data = bpy.data.cameras.new("Camera")
camera = bpy.data.objects.new("Camera", camera_data)
bpy.context.scene.collection.objects.link(camera)
camera.location = camera_location
look = (center - camera_location).normalized()
camera.rotation_euler = look.to_track_quat("-Z", "Y").to_euler()
camera_data.clip_start = max(distance * 0.01, 1e-5)
camera_data.clip_end = distance * 100
bpy.context.scene.camera = camera

sun_data = bpy.data.lights.new("Sun", type="SUN")
sun_data.energy = 3.0
sun = bpy.data.objects.new("Sun", sun_data)
bpy.context.scene.collection.objects.link(sun)
sun.rotation_euler = (math.radians(55), 0, math.radians(35))

for obj in meshes:
    material = bpy.data.materials.new(name=f"mat_{obj.name}")
    material.diffuse_color = (0.65, 0.72, 0.8, 1.0)
    obj.data.materials.append(material)

scene = bpy.context.scene
scene.render.engine = "BLENDER_EEVEE"
scene.render.resolution_x = 1600
scene.render.resolution_y = 1200
scene.render.image_settings.file_format = "PNG"
scene.render.filepath = png_path
scene.world = bpy.data.worlds.new("World")
scene.world.color = (0.9, 0.9, 0.92)

bpy.ops.render.render(write_still=True)
print(f"Rendered {len(meshes)} mesh objects to {png_path}")
print(f"Scene bounds: min={tuple(minimum)} max={tuple(maximum)}")
