#!/usr/bin/env python3
"""Makes the example's player model, demon.glb, from the demon model's source glb.

    python3 trim_demon.py <source demon_final.glb> [demon.glb]

Keeps the skinned body, head and hair, the skeleton that moves them and the clips in CLIPS;
drops every other mesh (the weapons, the face cards, the hat, the glasses), every texture and
image, the UVs that only the textures read, and every `extras` block. Two plain materials, no
textures: `demon` on the body and head, which the example colours per player, and `hair`. Plain
Python, no Blender: the
kept bytes (vertices, weights, bind matrices, keyframes) are copied as they are, never re-baked.
"""

import json
import struct
import sys

# The skinned meshes the example draws, by node name.
MESH_NODES = ["demon body", "demon head", "hair long"]
# The clips the example plays, by name: the run, the jump, and the pistol pose it stands in.
CLIPS = ["Armature|Running(1)", "Armature|Pistol Jump", "PistolShoot"]
# Which material each kept mesh gets: an index into MATERIALS.
MESH_MATERIAL = {"demon body": 0, "demon head": 0, "hair long": 1}
MATERIALS = [
    {"name": "demon", "pbrMetallicRoughness": {"metallicFactor": 0.0, "roughnessFactor": 0.8}},
    {
        "name": "hair",
        "pbrMetallicRoughness": {
            "baseColorFactor": [0.05, 0.04, 0.06, 1.0],
            "metallicFactor": 0.0,
            "roughnessFactor": 0.7,
        },
    },
]
# Vertex attributes kept: no UVs, nothing samples a texture.
ATTRIBUTES = ["POSITION", "NORMAL", "JOINTS_0", "WEIGHTS_0"]

GLB_MAGIC, JSON_CHUNK, BIN_CHUNK = 0x46546C67, 0x4E4F534A, 0x004E4942


def read_glb(path):
    data = open(path, "rb").read()
    magic, version, _ = struct.unpack_from("<III", data, 0)
    assert magic == GLB_MAGIC and version == 2, f"{path}: not a glTF 2 binary"
    offset, gltf, binary = 12, None, b""
    while offset < len(data):
        length, kind = struct.unpack_from("<II", data, offset)
        chunk = data[offset + 8 : offset + 8 + length]
        if kind == JSON_CHUNK:
            gltf = json.loads(chunk)
        elif kind == BIN_CHUNK:
            binary = chunk
        offset += 8 + length
    return gltf, binary


def write_glb(path, gltf, binary):
    text = json.dumps(gltf, separators=(",", ":")).encode()
    text += b" " * (-len(text) % 4)
    binary += b"\0" * (-len(binary) % 4)
    total = 12 + 8 + len(text) + 8 + len(binary)
    with open(path, "wb") as out:
        out.write(struct.pack("<III", GLB_MAGIC, 2, total))
        out.write(struct.pack("<II", len(text), JSON_CHUNK) + text)
        out.write(struct.pack("<II", len(binary), BIN_CHUNK) + binary)


def trim(src):
    nodes = src["nodes"]
    by_name = {node.get("name"): index for index, node in enumerate(nodes)}
    parent = {child: index for index, node in enumerate(nodes) for child in node.get("children", [])}
    skin = src["skins"][0]

    # The kept meshes, the skin's joints, and everything above them.
    keep = {by_name[name] for name in MESH_NODES} | set(skin["joints"])
    for index in list(keep):
        while index in parent:
            index = parent[index]
            keep.add(index)
    node_map = {old: new for new, old in enumerate(sorted(keep))}

    out = {"asset": {"version": "2.0", "generator": "trim_demon.py"}, "scene": 0}
    accessors, views, binary = [], [], bytearray()
    accessor_map = {}

    def copy_accessor(old):
        if old in accessor_map:
            return accessor_map[old]
        accessor = dict(src["accessors"][old])
        assert "sparse" not in accessor
        view = src["bufferViews"][accessor.pop("bufferView")]
        start = view.get("byteOffset", 0)
        binary.extend(b"\0" * (-len(binary) % 4))
        new_view = {"buffer": 0, "byteOffset": len(binary), "byteLength": view["byteLength"]}
        for key in ("byteStride", "target"):
            if key in view:
                new_view[key] = view[key]
        binary.extend(SOURCE_BIN[start : start + view["byteLength"]])
        views.append(new_view)
        accessor["bufferView"] = len(views) - 1
        accessor.pop("name", None)
        accessors.append(accessor)
        accessor_map[old] = len(accessors) - 1
        return accessor_map[old]

    meshes = []
    out["nodes"] = []
    for old in sorted(keep):
        node = {key: value for key, value in nodes[old].items() if key != "extras"}
        node["children"] = [node_map[child] for child in node.get("children", []) if child in keep]
        if not node["children"]:
            del node["children"]
        if "mesh" in node:
            mesh = src["meshes"][node["mesh"]]
            primitives = []
            for primitive in mesh["primitives"]:
                assert not primitive.get("targets"), "morph targets aren't copied"
                primitives.append(
                    {
                        "attributes": {
                            name: copy_accessor(index)
                            for name, index in primitive["attributes"].items()
                            if name in ATTRIBUTES
                        },
                        "indices": copy_accessor(primitive["indices"]),
                        "material": MESH_MATERIAL[node["name"]],
                        "mode": primitive.get("mode", 4),
                    }
                )
            meshes.append({"name": node["name"], "primitives": primitives})
            node["mesh"] = len(meshes) - 1
            node["skin"] = 0
        out["nodes"].append(node)

    out["meshes"] = meshes
    out["materials"] = MATERIALS
    out["skins"] = [
        {
            "name": skin.get("name", "Armature"),
            "joints": [node_map[joint] for joint in skin["joints"]],
            "inverseBindMatrices": copy_accessor(skin["inverseBindMatrices"]),
        }
    ]

    animations = {animation["name"]: animation for animation in src["animations"]}
    missing = [name for name in CLIPS if name not in animations]
    assert not missing, f"the source has no clip {missing}; it has {sorted(animations)}"
    out["animations"] = []
    for name in CLIPS:
        animation = animations[name]
        samplers, channels = [], []
        for channel in animation["channels"]:
            target = channel["target"]
            if target["node"] not in keep:
                continue
            sampler = animation["samplers"][channel["sampler"]]
            samplers.append(
                {
                    "input": copy_accessor(sampler["input"]),
                    "output": copy_accessor(sampler["output"]),
                    "interpolation": sampler.get("interpolation", "LINEAR"),
                }
            )
            channels.append(
                {
                    "sampler": len(samplers) - 1,
                    "target": {"node": node_map[target["node"]], "path": target["path"]},
                }
            )
        out["animations"].append({"name": name, "channels": channels, "samplers": samplers})

    roots = [node_map[index] for index in src["scenes"][src.get("scene", 0)]["nodes"]]
    out["scenes"] = [{"name": "Scene", "nodes": roots}]
    out["accessors"] = accessors
    out["bufferViews"] = views
    out["buffers"] = [{"byteLength": len(binary) + (-len(binary) % 4)}]
    return out, bytes(binary)


if __name__ == "__main__":
    if len(sys.argv) not in (2, 3):
        sys.exit(__doc__)
    source, SOURCE_BIN = read_glb(sys.argv[1])
    trimmed, trimmed_bin = trim(source)
    target = sys.argv[2] if len(sys.argv) == 3 else "demon.glb"
    write_glb(target, trimmed, trimmed_bin)
    print(f"{target}: {sum(len(m['primitives']) for m in trimmed['meshes'])} primitives, "
          f"{len(trimmed['animations'])} clips, {len(trimmed_bin)} bytes of buffer")
