"""Bake corrected photo projections onto a dedicated runtime UV atlas."""

import argparse
import sys
from pathlib import Path

import bpy

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--output", type=Path, required=True)
args = parser.parse_args(sys.argv[sys.argv.index("--") + 1 :])
R = args.output
R.mkdir(parents=True, exist_ok=False)
o = bpy.data.objects["Measured video head"]
mesh = o.data
original_uv = mesh.uv_layers.active.name
for material in mesh.materials:
    if not material.use_nodes:
        continue
    nodes = material.node_tree.nodes
    links = material.node_tree.links
    uv = nodes.new("ShaderNodeUVMap")
    uv.uv_map = original_uv
    for node in list(nodes):
        if node.type == "TEX_IMAGE" and not node.inputs["Vector"].is_linked:
            links.new(uv.outputs["UV"], node.inputs["Vector"])
mesh.uv_layers.new(name="RuntimeUV")
mesh.uv_layers.active_index = len(mesh.uv_layers) - 1
mesh.uv_layers.active.active_render = True
bpy.ops.object.select_all(action="DESELECT")
o.select_set(True)
bpy.context.view_layer.objects.active = o
bpy.ops.object.mode_set(mode="EDIT")
bpy.ops.mesh.select_all(action="SELECT")
bpy.ops.uv.smart_project(angle_limit=1.151917, island_margin=0.003)
bpy.ops.object.mode_set(mode="OBJECT")
atlas = bpy.data.images.new("Runtime portrait atlas", 4096, 4096, alpha=False)
for mat in mesh.materials:
    if not mat.use_nodes:
        continue
    n = mat.node_tree.nodes.new("ShaderNodeTexImage")
    n.image = atlas
    mat.node_tree.nodes.active = n
s = bpy.context.scene
s.render.engine = "CYCLES"
s.cycles.samples = 1
s.render.bake.margin = 8
bpy.ops.object.bake(type="EMIT")
atlas.filepath_raw = str(R / "runtime-portrait-atlas.png")
atlas.file_format = "PNG"
atlas.save()
atlas.pack()
mat = bpy.data.materials.new("Baked runtime portrait")
mat.use_nodes = True
n = mat.node_tree.nodes
n.clear()
out = n.new("ShaderNodeOutputMaterial")
em = n.new("ShaderNodeEmission")
tex = n.new("ShaderNodeTexImage")
tex.image = atlas
uv = n.new("ShaderNodeUVMap")
uv.uv_map = "RuntimeUV"
mat.node_tree.links.new(uv.outputs[0], tex.inputs[0])
mat.node_tree.links.new(tex.outputs["Color"], em.inputs["Color"])
mat.node_tree.links.new(em.outputs[0], out.inputs[0])
mesh.materials.clear()
mesh.materials.append(mat)
for polygon in mesh.polygons:
    polygon.material_index = 0
bpy.ops.wm.save_as_mainfile(filepath=str(R / "video-model-runtime-baked.blend"))
