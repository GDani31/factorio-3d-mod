# fix "solid" foliage prims that patch_tree_alpha.py accidentally masked out.
#
# some FUE5 trees (tree1) share ONE diffuse+mask between the leaf cards and the
# solid trunk. patch_tree_alpha bakes the LEAF opacity mask into that shared
# texture, but the trunk's UVs sit in a region the leaf mask marks fully
# transparent — so the shader's alpha cutout discards the whole trunk ("trees
# don't render their logs"). the bark RGB is still there; only the alpha is 0.
#
# this splits any textured prim whose UVs sample ~0 alpha everywhere (i.e. the
# whole prim is being discarded) onto a fresh OPAQUE copy of its texture, so it
# renders its bark again while the leaf cards keep their cutout. idempotent:
# a prim already pointing at an "*_opaque" image is left alone.
#
# usage: python tools/fix_solid_prims.py   (run AFTER patch_tree_alpha.py)

import io
import json
import os
import struct

from PIL import Image

MODELS = os.path.join(os.path.dirname(__file__), "..", "models", "ENTITIES", "FOLIAGE")

CT = {5120: ("b", 1), 5121: ("B", 1), 5122: ("h", 2), 5123: ("H", 2), 5125: ("I", 4), 5126: ("f", 4)}
NC = {"SCALAR": 1, "VEC2": 2, "VEC3": 3, "VEC4": 4}


def read_glb(path):
    data = open(path, "rb").read()
    assert struct.unpack("<I", data[:4])[0] == 0x46546C67, "not a glb"
    off, js, binc = 12, None, None
    while off < len(data):
        clen, ctype = struct.unpack("<II", data[off : off + 8])
        chunk = data[off + 8 : off + 8 + clen]
        if ctype == 0x4E4F534A:
            js = json.loads(chunk)
        elif ctype == 0x004E4942:
            binc = bytearray(chunk)
        off += 8 + clen
    return js, binc


def write_glb(path, js, binc):
    jb = json.dumps(js, separators=(",", ":")).encode()
    jb += b" " * (-len(jb) % 4)
    while len(binc) % 4:
        binc.append(0)
    total = 12 + 8 + len(jb) + 8 + len(binc)
    with open(path, "wb") as f:
        f.write(struct.pack("<III", 0x46546C67, 2, total))
        f.write(struct.pack("<II", len(jb), 0x4E4F534A))
        f.write(jb)
        f.write(struct.pack("<II", len(binc), 0x004E4942))
        f.write(bytes(binc))


def uvs_of(js, binc, acc_idx):
    a = js["accessors"][acc_idx]
    bv = js["bufferViews"][a["bufferView"]]
    off = bv.get("byteOffset", 0) + a.get("byteOffset", 0)
    ct, sz = CT[a["componentType"]]
    nc = NC[a["type"]]
    stride = bv.get("byteStride", sz * nc)
    for i in range(a["count"]):
        yield struct.unpack_from("<" + ct * nc, binc, off + i * stride)


def image_rgba(js, binc, img_idx):
    bv = js["bufferViews"][js["images"][img_idx]["bufferView"]]
    o = bv.get("byteOffset", 0)
    return Image.open(io.BytesIO(bytes(binc[o : o + bv["byteLength"]]))).convert("RGBA")


def append_opaque_image(js, binc, src_img_idx):
    """append an alpha=255 copy of image src_img_idx, return its new index"""
    rgba = image_rgba(js, binc, src_img_idx)
    rgba.putalpha(255)
    out = io.BytesIO()
    rgba.save(out, "PNG", optimize=True)
    png = out.getvalue()
    while len(binc) % 4:
        binc.append(0)
    js["bufferViews"].append({"buffer": 0, "byteOffset": len(binc), "byteLength": len(png)})
    binc.extend(png)
    name = js["images"][src_img_idx].get("name", "img") + "_opaque"
    js["images"].append({"name": name, "mimeType": "image/png", "bufferView": len(js["bufferViews"]) - 1})
    return len(js["images"]) - 1


def fix_glb(path):
    js, binc = read_glb(path)
    textures = js.get("textures", [])
    images = js.get("images", [])
    opaque_for = {}  # src image idx -> new opaque image idx (built lazily)
    fixed = []
    for node in js.get("nodes", []):
        if node.get("mesh") is None:
            continue
        for prim in js["meshes"][node["mesh"]]["primitives"]:
            mat = prim.get("material")
            uvacc = prim["attributes"].get("TEXCOORD_0")
            if mat is None or uvacc is None:
                continue
            bt = js["materials"][mat].get("pbrMetallicRoughness", {}).get("baseColorTexture")
            if bt is None:
                continue
            src_img = textures[bt["index"]]["source"]
            if images[src_img].get("name", "").endswith("_opaque"):
                continue  # already fixed
            img = image_rgba(js, binc, src_img)
            W, H = img.size
            px = img.load()
            # opaque coverage over the prim's whole UV BOUNDING BOX, not just at
            # the vertices: a leaf card's verts all sit on transparent edges
            # (would read 0), but its interior is opaque and renders fine. only
            # a solid mesh whose ENTIRE uv region is masked away (a trunk sharing
            # a leaf mask) has ~0% opaque area — that's what we repair.
            us = [u for (u, _v) in uvs_of(js, binc, uvacc)]
            vs = [v for (_u, v) in uvs_of(js, binc, uvacc)]
            if not us:
                continue
            u0, u1, v0, v1 = min(us), max(us), min(vs), max(vs)
            opaque = 0
            grid = 48
            for i in range(grid):
                for j in range(grid):
                    u = u0 + (u1 - u0) * i / (grid - 1)
                    v = v0 + (v1 - v0) * j / (grid - 1)
                    x = int((u % 1.0) * (W - 1))
                    y = int((v % 1.0) * (H - 1))
                    opaque += px[x, y][3] >= 128
            if opaque > grid * grid * 0.01:
                continue  # >1% of its texture region renders — a real card
            # every vertex is masked out: this solid prim would fully discard.
            # repoint its material at an opaque copy of the same texture.
            if src_img not in opaque_for:
                opaque_for[src_img] = append_opaque_image(js, binc, src_img)
            new_tex = {"sampler": textures[bt["index"]].get("sampler", 0), "source": opaque_for[src_img]}
            textures.append(new_tex)
            bt["index"] = len(textures) - 1
            fixed.append(node.get("name", "?"))
    if fixed:
        js["textures"] = textures
        js["buffers"][0]["byteLength"] = len(binc) + (-len(binc) % 4)
        write_glb(path, js, binc)
    return fixed


def main():
    for tree in sorted(os.listdir(MODELS)):
        glb = os.path.join(MODELS, tree, "static.glb")
        if not os.path.exists(glb):
            continue
        fixed = fix_glb(glb)
        if fixed:
            print(f"{tree}: made opaque -> {fixed}")


if __name__ == "__main__":
    main()
