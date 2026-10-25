# Nozavolochen (/ˌnəʊzəvəˈlɒtʃɛn/, "convergence")

One `xinput9_1_0.dll` hosts any number of independent spec-driven
runtime mods for No Man's Sky. Each mod is a folder with a
`mod.toml` describing pattern patches the loader applies at game
boot.

**Docs**: https://nozavolochen.github.io/docs/

## Mod distribution

The framework ships as its own [release](https://www.nexusmods.com/games/nomanssky/mods/4700?tab=files): an archive with
`Binaries/xinput9_1_0.dll`, marked as a required dependency on Nexus by your mod. Mods
ship only their `mod.toml` (plus static files if required) and never bundle
the DLL.

## Build

```sh
make loader   # cross-build xinput9_1_0.dll (mingw on Linux)
make check    # verify embedded specs against NMS.exe, read-only
```
