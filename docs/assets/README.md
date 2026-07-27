# Assets

`logo-dark.svg` / `logo-light.svg` — the wordmark, generated from the TUI's own
gradient (`LOGO_GRADIENT_DARK` / `_LIGHT` in `tui_tea/view.rs`). The README picks
one per `prefers-color-scheme`, so both must stay in step with the palette.

`demo.gif` — the TUI recording at the top of the project README.

## Replacing the demo

A screen recording converts like this — two passes, because a generated palette
is far kinder to the gold gradient than the default 256 colours:

```sh
ffmpeg -i recording.mp4 \
  -vf "fps=10,scale=720:-1:flags=lanczos,palettegen=stats_mode=diff" -y pal.png
ffmpeg -i recording.mp4 -i pal.png \
  -lavfi "fps=10,scale=720:-1:flags=lanczos,paletteuse=dither=bayer:bayer_scale=3" \
  -y demo.gif
```

Or record the terminal directly, which gives crisp text at a fraction of the size
because it captures characters rather than pixels:

```sh
asciinema rec demo.cast
agg demo.cast demo.gif --font-size 16 --theme asciinema
```

**Watch the size.** A binary committed here is permanent — every clone carries it
forever, and removing it later means rewriting history. GitHub warns above 10 MB.
For reference, the current 31-second demo is 8.7 MB at 720px/10fps; the same clip
trimmed to 15 seconds would be about 5 MB. Shorter is usually better anyway.
