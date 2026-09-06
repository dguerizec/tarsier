"""Export unposed triangle streams and facial deltas for the OpenGL runtime."""

import argparse
import json
import sys
from pathlib import Path

import bpy
import numpy as np

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--revision", required=True)
args = parser.parse_args(sys.argv[sys.argv.index("--") + 1 :])
OUT = args.output
OUT.mkdir(parents=True, exist_ok=False)
names = [
    "eyeBlinkLeft",
    "eyeBlinkRight",
    "jawOpen",
    "mouthSmileLeft",
    "mouthSmileRight",
    "browInnerUp",
    "browOuterUpLeft",
    "browOuterUpRight",
]
manifest = {
    "schema_version": 1,
    "revision": args.revision,
    "morphs": names,
    "draws": [],
}
arrays = {}
index = 0
for obj in bpy.context.scene.objects:
    if obj.type != "MESH" or obj.hide_render:
        continue
    if obj.name not in [
        "Measured video head",
        "Beard edge fibers",
        "Mouth cavity",
    ] and not obj.name.startswith("Upper tooth"):
        continue
    mesh = obj.data
    mesh.calc_loop_triangles()
    matrix = np.array(obj.matrix_world)
    co = np.array([v.co[:] for v in mesh.vertices])
    world = co @ matrix[:3, :3].T + matrix[:3, 3]
    deltas = np.zeros((len(co), 8, 3), np.float32)
    if mesh.shape_keys:
        for k, name in enumerate(names):
            if name in mesh.shape_keys.key_blocks:
                deltas[:, k] = (
                    np.array([v.co[:] for v in mesh.shape_keys.key_blocks[name].data])
                    - co
                ) @ matrix[:3, :3].T
    group = obj.vertex_groups.get("Head")
    weights = np.zeros(len(co), np.float32)
    if group:
        for v in mesh.vertices:
            for g in v.groups:
                if g.group == group.index:
                    weights[v.index] = g.weight
    for material_index, material in enumerate(mesh.materials):
        tris = [t for t in mesh.loop_triangles if t.material_index == material_index]
        if not tris:
            continue
        ids = np.array([v for t in tris for v in t.vertices])
        loops = [l for t in tris for l in t.loops]
        uv = (
            np.array([mesh.uv_layers.active.data[l].uv[:] for l in loops])
            if mesh.uv_layers.active
            else np.zeros((len(ids), 2))
        )
        data = np.concatenate(
            [world[ids], uv, weights[ids, None], deltas[ids].reshape(len(ids), 24)],
            axis=1,
        ).astype(np.float32)
        arrays[f"draw{index}"] = data
        photo = next(
            (
                n.image
                for n in material.node_tree.nodes
                if n.type == "TEX_IMAGE" and n.image
            ),
            None,
        )
        color = [1, 1, 1]
        if photo:
            image_name = f"texture-{index}.png"
            photo.save_render(str(OUT / image_name))
        else:
            image_name = None
            em = next(
                (n for n in material.node_tree.nodes if n.type == "EMISSION"), None
            )
            if em:
                color = list(em.inputs["Color"].default_value[:3])
        manifest["draws"].append(
            {
                "array": f"draw{index}",
                "texture": image_name,
                "color": color,
                "object": obj.name,
            }
        )
        index += 1
np.savez_compressed(OUT / "mesh.npz", **arrays)
(OUT / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(
    "Exported",
    index,
    "draws",
    sum(len(a) for a in arrays.values()),
    "triangle vertices",
)
