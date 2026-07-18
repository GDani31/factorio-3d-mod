# builds the 5 per-state player glbs from the mixamo fbx files.
# Idle.fbx carries the skinned mesh; the other fbx are animation-only and
# share the same mixamorig skeleton, so their actions retarget by assignment.
# run: blender --background --python export_player.py
import bpy, os, sys

PLAYER = r"M:\SteamLibrary\steamapps\common\Factorio\3d_mod_models\player"
OUT = r"M:\SteamLibrary\steamapps\common\Factorio\3d_mod_models\models\ENTITIES\PLAYER"

CLIPS = [
    ("idle", "Idle.fbx"),
    ("running", "Running.fbx"),
    ("shoot-run", "Run Forward.fbx"),
    ("shoot-stand", "Firing Rifle.fbx"),
    ("mining", "Standing Melee Attack Downward.fbx"),
]

def fcurve_count(act):
    n = len(getattr(act, "fcurves", []) or [])
    if n == 0:
        for layer in getattr(act, "layers", []):
            for strip in layer.strips:
                for bag in strip.channelbags:
                    n += len(bag.fcurves)
    return n

bpy.ops.wm.read_factory_settings(use_empty=True)
bpy.context.scene.render.fps = 30  # mixamo clips are 30fps

# base: the skinned mesh + skeleton + idle action
bpy.ops.import_scene.fbx(filepath=os.path.join(PLAYER, "Idle.fbx"))
arm = next(o for o in bpy.data.objects if o.type == 'ARMATURE')
meshes = [o for o in bpy.data.objects if o.type == 'MESH']
actions = {"idle": arm.animation_data.action}
actions["idle"].use_fake_user = True

for state, fbx in CLIPS[1:]:
    before = set(bpy.data.actions)
    bpy.ops.import_scene.fbx(filepath=os.path.join(PLAYER, fbx))
    new_acts = [a for a in bpy.data.actions if a not in before]
    act = max(new_acts, key=fcurve_count)
    act.use_fake_user = True
    act.name = state
    actions[state] = act
    # drop the animation-only import (armature without mesh)
    for ob in [o for o in bpy.data.objects if o not in meshes and o is not arm]:
        bpy.data.objects.remove(ob, do_unlink=True)

# materials: body near-white (the player-color tint multiplies onto it),
# joints dark. no textures on the mixamo bot, just principled colors
for mesh in meshes:
    for mat in mesh.data.materials:
        if not mat or not mat.use_nodes:
            continue
        bsdf = next((n for n in mat.node_tree.nodes if n.type == 'BSDF_PRINCIPLED'), None)
        if not bsdf:
            continue
        if 'Body' in mat.name or 'Surface' in mesh.name:
            bsdf.inputs['Base Color'].default_value = (0.85, 0.85, 0.85, 1.0)
        else:
            bsdf.inputs['Base Color'].default_value = (0.12, 0.12, 0.13, 1.0)
        bsdf.inputs['Metallic'].default_value = 0.1
        bsdf.inputs['Roughness'].default_value = 0.55

os.makedirs(OUT, exist_ok=True)
for state, _ in CLIPS:
    act = actions[state]
    arm.animation_data.action = act
    # blender 5.x slotted actions: the slot must be assigned too or nothing evaluates
    if getattr(act, "slots", None) and len(act.slots):
        arm.animation_data.action_slot = act.slots[0]
    fr = act.frame_range
    bpy.context.scene.frame_start = int(fr[0])
    bpy.context.scene.frame_end = int(fr[1])
    bpy.ops.object.select_all(action='DESELECT')
    arm.select_set(True)
    for m in meshes:
        m.select_set(True)
    path = os.path.join(OUT, f"{state}.glb")
    bpy.ops.export_scene.gltf(
        filepath=path,
        use_selection=True,
        export_format='GLB',
        export_apply=False,
        export_skins=True,
        export_yup=True,
        export_animations=True,
        export_animation_mode='ACTIVE_ACTIONS',
        export_bake_animation=False,
        export_optimize_animation_size=False,
    )
    print(f"EXPORTED {path} frames {fr[0]}-{fr[1]}")

# verify: re-import each glb, check anim + skin presence
for state, _ in CLIPS:
    bpy.ops.wm.read_factory_settings(use_empty=True)
    path = os.path.join(OUT, f"{state}.glb")
    bpy.ops.import_scene.gltf(filepath=path)
    n_arm = sum(1 for o in bpy.data.objects if o.type == 'ARMATURE')
    n_mesh = sum(1 for o in bpy.data.objects if o.type == 'MESH')
    n_act = len(bpy.data.actions)
    size = os.path.getsize(path)
    print(f"VERIFY {state}.glb: {n_arm} armature, {n_mesh} meshes, {n_act} actions, {size} bytes")

print("EXPORT DONE")
