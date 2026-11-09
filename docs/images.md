# Images

Programs running in Proteus can show images inline with the kitty graphics
protocol, Sixel, or iTerm2 inline images. Proteus also ships `proteus +icat`,
a small tool that shows image files and URLs. Images scroll with the text and
can be opened in a full-window viewer.

## Supported protocols

| Protocol | Sequences | Supported |
|----------|-----------|-----------|
| kitty graphics | `APC G ... ST` | Transmit (`a=t`, `a=T`), query (`a=q`), place (`a=p`), delete (`a=d`), animation frames (`a=f`), animation control (`a=a`), frame composition (`a=c`). Pixel formats `f=24`, `f=32` and PNG (`f=100`), zlib compression (`o=z`), chunked transfers, relative placements (`P=`, `Q=`), Unicode placeholders (`U=1` with `U+10EEEE` cells). |
| Sixel | `DCS ... q ... ST` | 256 color registers, images up to 16384 pixels on a side. |
| iTerm2 | `OSC 1337 ; File=... ST` | Inline images (`inline=1`), `width` and `height` in cells, `px`, `%` or `auto`, `preserveAspectRatio`, multipart transfers (`MultipartFile`, `FilePart`, `FileEnd`), `ReportCellSize`. File downloads (`inline=0`) are not supported. |

Compressed image data (PNG, JPEG, GIF, WebP) is decoded in the background, so
a large image does not stall the terminal. Animated GIFs sent as iTerm2 inline
images play their frames.

Unicode placeholders are ordinary text characters, so images placed with
them scroll, reflow and pass through multiplexers such as tmux like any other
text.

## Show an image with `proteus +icat`

```sh
proteus +icat photo.jpg
proteus +icat https://example.com/logo.png
proteus +icat --place 40x20@0x0 diagram.png
proteus +icat --clear
```

```
Usage: proteus +icat [OPTIONS] [FILE|URL]...
```

| Option | Effect |
|--------|--------|
| `FILE` or `URL` | PNG, JPEG, GIF or WebP files, or `http://` / `https://` URLs. URLs are fetched with `curl` (up to 256 MiB). Several may be given. |
| `--place COLSxROWS@XxY` | Draw in a box of `COLS` by `ROWS` cells with its top-left corner at column `X`, row `Y` (counted from 0). The cursor does not move. |
| `--scale` | Grow small images to fill the terminal width (or the `--place` box). Without it, images are only shrunk to fit. |
| `--clear` | Delete all images in the terminal before showing any files. Can be used alone. |
| `--inline` | Always send the image data in the escape sequences, even for local files. |

Without `--place`, the image fits in the terminal width and one row less than
its height, keeps its aspect ratio, and the cursor moves below it.

How the image is sent:

- A local PNG file is sent as a file path, and Proteus reads the file itself.
  This is skipped when `SSH_CONNECTION`, `SSH_CLIENT` or `SSH_TTY` is set, or
  with `--inline`.
- Other formats are converted to PNG, and all data is sent inline.

`proteus +icat` exits with status 1 if any file failed, and prints the reason
on standard error. It uses only the kitty graphics protocol.

If an image does not appear, see
[Local files and remote panes](#local-files-and-remote-panes): when Proteus
refuses a file path, nothing is shown. Use `--inline` in that case.

## View an image

Ctrl+click an image to open it over the whole window. The `image.view` action
opens the lowest (most recent) image on the focused pane's screen; it has no
default key:

```lua
require("proteus").config { keys = { normal = { ["ctrl+shift+o"] = "image.view" } } }
```

The bar at the bottom shows the size and zoom, for example `IMAGE 1920×1080 50%`.

| Key or mouse | Effect |
|--------------|--------|
| Wheel | Zoom around the pointer |
| `+` or `=` / `-` | Zoom in / out |
| `0` | Fit the window (never larger than 100%) |
| `1` | Actual size (100%) |
| Drag, arrows, `h` `j` `k` `l` | Pan (`Shift` with arrows or `hjkl` moves 4 times as far) |
| `c` | Copy the image to the clipboard as PNG |
| `s` | Save it as PNG |
| `Esc`, `q`, right click | Close the viewer |

`s` saves to your pictures directory (from `xdg-user-dir PICTURES`, else
`~/Pictures` if it exists, else your home directory) as
`proteus-UNIXTIME.png`, adding `-2`, `-3`, ... if the name is taken. The bar
shows the saved path.

The viewer keeps its own copy of the image, so it stays the same even if the
program changes or deletes the image meanwhile.

## Configure graphics

```lua
require("proteus").config {
  graphics = {
    enabled = true,
    max_image = 64,       -- MiB
    pane_memory = 256,    -- MiB
    total_memory = 1024,  -- MiB
    gpu_memory = 512,     -- MiB
    local_files = true,
  },
}
```

| Option | Default | Meaning |
|--------|---------|---------|
| `graphics.enabled` | `true` | Accept image protocols at all. With `false`, kitty graphics, Sixel and iTerm2 images are ignored. |
| `graphics.max_image` | `64` | Largest single image after decoding, in MiB, counting all animation frames. |
| `graphics.pane_memory` | `256` | Decoded image memory per pane, in MiB, for its normal and alternate screens together. |
| `graphics.total_memory` | `1024` | Decoded image memory of all panes together, in MiB. |
| `graphics.gpu_memory` | `512` | GPU texture memory per window, in MiB. |
| `graphics.local_files` | `true` | Let local programs send kitty images as file paths, temporary files or shared memory. |

Sizes are numbers in MiB, at least 1 (fractions are allowed). Changes to
`enabled`, `max_image`, `pane_memory` and `local_files` apply to panes opened
after the config is loaded. `gpu_memory` applies to all windows on reload.
`total_memory` is read when Proteus starts.

## Memory limits

Decoded images use 4 bytes per pixel. A 4096 by 4096 image takes 64 MiB, the
default `max_image`, and an animation needs that much for every frame.

| Limit | What happens |
|-------|--------------|
| An image is larger than `max_image` | It is refused. kitty clients get `EFBIG`. For GIFs, frames past the limit are dropped. |
| A pane or all panes together would go over `pane_memory` / `total_memory` | Proteus first deletes images that are not placed anywhere, then removes the pixels of the images displayed least recently. Their place on screen stays and shows `image evicted`. If nothing can be freed, the new image is refused (`ENOSPC` for kitty clients). |
| A window's images need more GPU memory than `gpu_memory` | The least recently drawn images are dropped from the GPU and uploaded again when they are drawn next. Nothing visible is lost. |

Each screen holds at most 4096 images and 4096 placements, and an image has
at most 4096 animation frames. Images are dropped together with the
scrollback lines they belong to.

If you work with large images and see `image evicted`, raise `pane_memory`
and `total_memory`. If one image is refused, raise `max_image`.

## Local files and remote panes

The kitty protocol lets a program send the name of a file, a temporary file or
a shared memory object instead of the image data (`t=f`, `t=t`, `t=s`). The
terminal then reads it from your machine. A program on another machine could
use this to make Proteus read and display your local files, so Proteus only
accepts these names from programs it knows run locally.

Proteus refuses them (kitty clients get `EBADF`) when:

- the pane is an SSH pane opened by Proteus (see [ssh.md](ssh.md)),
- any process in the pane's shell session, or started from the shell, is one
  of: `ssh`, `autossh`, `sshpass`, `mosh`, `mosh-client`, `et`, `telnet`,
  `rlogin`, `nc`, `ncat`, `socat`, `docker`, `podman`, `kubectl`, `lxc`,
  `incus`, `machinectl`, `toolbox`, `distrobox`, `vagrant`, `minicom`,
  `picocom`, `tsh`, `gcloud`, `aws`, `session-manager`, `waypipe`, `tmux`,
  `screen`, `zellij`, `abduco`, `dtach`,
- the shell reported a working directory on another host (OSC 7),
- `graphics.local_files` is `false`.

Multiplexers are in the list because their sessions can be attached from
anywhere. The check covers the whole session, so an `ssh` running in the
background also counts. Once those programs have exited, local file names are
accepted again.

Images sent as data (`t=d`, Sixel, iTerm2) are not affected and work in all
panes, including over SSH. Tools that send local file names need an option to
send data instead; for `proteus +icat` that is `--inline`.

To refuse file names everywhere:

```lua
require("proteus").config { graphics = { local_files = false } }
```
