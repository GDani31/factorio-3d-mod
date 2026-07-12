# factorio 3d mod

a dll that gets injected into factorio (2.0.77, windows, dx11) and turns the flat game into a 3d view. the finished frame is warped onto a tilted ground plane for the 3d perspective, and buildings/players/trains are captured into their own texture and drawn as upright billboards standing on that ground (belts and elevated rails float above it). structures made of multiple sprites/tiles are grouped by their map position so all parts share one floor line and stack into one solid standing object instead of splitting apart.

## controls

- shift + right-drag (or just middle-mouse drag): rotate / tilt the camera
- shift + scroll: 3d zoom out / back in
- shift + scroll in at closest zoom: first person — mouse looks around, hold ctrl to free the cursor, scroll out to exit

## install

easy way: grab `factorio_3d.dll` + `inject.exe` from the releases tab. start factorio, load a save, run `inject.exe`. re-inject after every game restart.

build it yourself: install rust nightly, then

```
cargo build --release
cargo run --release -p injector   # with factorio running
```

logs go to `%APPDATA%\Factorio\factorio_3d.log`. if a game update breaks the mod, the version-specific addresses live in `factorio_3d/src/offsets.rs`.
