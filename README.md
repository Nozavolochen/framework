# Nozavolochen (/ˌnəʊzəvəˈlɒtʃɛn/, "convergence")

One `xinput9_1_0.dll` hosts any number of independent spec-driven
runtime mods for No Man's Sky. Each mod is a folder with a
`mod.toml` describing pattern patches the loader applies at game
boot.

**Docs**: https://nozavolochen.github.io/docs/

## Mod distribution

The framework ships as its own [release](https://www.nexusmods.com/games/nomanssky/mods/4700?tab=files): an archive with
flat `xinput9_1_0.dll` at the root, marked as a required dependency on Nexus by your mod.

- **Vortex:** install with no extra steps. It routes the DLL into `Binaries/` for you.
- **Amethyst (Routing Rule, one-time):** Data tab > Routing Rules (bottom, near Search) > Add > Destinations: `Binaries`, Extensions: `.dll` > Save, then deploy.
- **Manual install (no manager):** drop `xinput9_1_0.dll` into the game's `Binaries/` folder.

Mods ship only their `mod.toml` (plus static files if required) and never bundle
the DLL.

## Strings

Overlay chrome lives in `loader/strings/<code>.json`: one flat object per
language, every file with the same keys as `en.json`. English is built into
the DLL and is the fallback for missing keys and unknown languages. The game
language is read from `Binaries/SETTINGS/GCUSERSETTINGSDATA.MXML` (Steam
`appmanifest_275850.acf` when the game follows Steam), so a game patch cannot
break detection.

`loader/strings` is the Weblate component root: translators edit the JSON
there, never Rust. A new file there ships in the next DLL build with no
Rust edit. The `nozavolochen-strings` crate (`loader/crates/strings`) holds
the lookup with English fallback plus the game-language detection, and its
unit tests run on the host:

```sh
make lang-check   # keys, value lengths, language codes
```

## Build

```sh
make loader   # cross-build xinput9_1_0.dll (mingw on Linux)
```
