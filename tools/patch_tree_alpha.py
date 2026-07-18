# bake the FUE5 per-material alpha masks into the tree glbs.
#
# the c4d/unreal tree assets store opacity as a SEPARATE *_alpha1.jpg (jpeg
# diffuse can't carry alpha), and the glb export dropped it — so foliage
# cards/blobs render their white background as solid geometry (white-blob
# trees). this rewrites each glb's diffuse images as RGBA pngs with the
# matching mask in the alpha channel; the model shader discards a<0.5.
#
# usage: python tools/patch_tree_alpha.py [fue5_foliage_dir]
#   default source: G:/projects/python/FUE5/Content/MyStuff/ENTITIES/FOLIAGE

import io
import json
import os
import struct
import sys

from PIL import Image

MODELS = os.path.join(os.path.dirname(__file__), "..", "models", "ENTITIES", "FOLIAGE")
DEFAULT_SRC = "G:/projects/python/FUE5/Content/MyStuff/ENTITIES/FOLIAGE"

# glb image name -> mask file (searched in the tree's FUE5 source dir)
def mask_for(img_name):
    if img_name == "Fastcolor":
        return "Fastmask.jpg"
    if "_diffuse" in img_name:
        return img_name.replace("_diffuse", "_alpha") + ".jpg"
    return None


def read_glb(path):
    with open(path, "rb") as f:
        data = f.read()
    magic, ver, _total = struct.unpack("<III", data[:12])
    assert magic == 0x46546C67, "not a glb"
    off = 12
    js = binc = None
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
    jbytes = json.dumps(js, separators=(",", ":")).encode()
    jbytes += b" " * (-len(jbytes) % 4)
    while len(binc) % 4:
        binc.append(0)
    total = 12 + 8 + len(jbytes) + 8 + len(binc)
    with open(path, "wb") as f:
        f.write(struct.pack("<III", 0x46546C67, 2, total))
        f.write(struct.pack("<II", len(jbytes), 0x4E4F534A))
        f.write(jbytes)
        f.write(struct.pack("<II", len(binc), 0x004E4942))
        f.write(bytes(binc))


def patch_tree(glb_path, src_dir):
    js, binc = read_glb(glb_path)
    patched = 0
    for img in js.get("images", []):
        mask_name = mask_for(img.get("name", ""))
        if not mask_name:
            continue
        mask_path = os.path.join(src_dir, mask_name)
        if not os.path.exists(mask_path):
            continue
        bv = js["bufferViews"][img["bufferView"]]
        start = bv.get("byteOffset", 0)
        blob = bytes(binc[start : start + bv["byteLength"]])
        diffuse = Image.open(io.BytesIO(blob)).convert("RGB")
        mask = Image.open(mask_path).convert("L").resize(diffuse.size, Image.BILINEAR)
        rgba = diffuse.copy()
        rgba.putalpha(mask)
        out = io.BytesIO()
        rgba.save(out, "PNG", optimize=True)
        png = out.getvalue()
        # append the png as a fresh bufferView; the old jpeg bytes stay as
        # dead weight (harmless, keeps every other bufferView offset valid)
        while len(binc) % 4:
            binc.append(0)
        js["bufferViews"].append({"buffer": 0, "byteOffset": len(binc), "byteLength": len(png)})
        binc.extend(png)
        img["bufferView"] = len(js["bufferViews"]) - 1
        img["mimeType"] = "image/png"
        patched += 1
        print(f"  {img['name']}: + alpha from {mask_name} ({diffuse.size[0]}x{diffuse.size[1]})")
    if patched:
        js["buffers"][0]["byteLength"] = len(binc) + (-len(binc) % 4)
        write_glb(glb_path, js, binc)
    return patched


def main():
    src_root = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_SRC
    total = 0
    for tree in sorted(os.listdir(MODELS)):
        glb = os.path.join(MODELS, tree, "static.glb")
        src = os.path.join(src_root, tree)
        if not os.path.exists(glb) or not os.path.isdir(src):
            continue
        print(f"{tree}:")
        total += patch_tree(glb, src)
    print(f"patched {total} images")


if __name__ == "__main__":
    main()
