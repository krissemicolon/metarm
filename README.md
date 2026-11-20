# metarm
> rm for metadata

`metarm` strips identifying metadata (camera make, model and serial number, GPS location, dates, XMP, comments) from images. You use it just like `rm`.

```sh
cargo install metarm
```

## Usage

```sh
metarm photo.jpg            # strip one file
metarm a.jpg b.png c.webp   # several files
metarm -rf ~/Pictures/out   # every image in a folder, recursively
metarm -a photo.jpg         # remove everything, including orientation
```

| Flag | Meaning |
| --- | --- |
| `-r`, `-R`, `--recursive` | Process directories recursively. Non-image files inside are skipped. |
| `-f`, `--force` | Does nothing. Accepted so `metarm -rf` works like `rm -rf`. |
| `-a`, `--all` | Also remove orientation, resolution and colour space (kept by default). |
| `-v`, `--verbose` | Print each file as it is processed. |
| `-h`, `--help` | Show help. |
| `-V`, `--version` | Show the version. |

## What it does

- Supports **JPEG, PNG (including animated) and WebP**, detected by content rather than file extension.
- Edits files in place without re-encoding, so image quality is unchanged.
- By default, keeps the tags that only affect display (orientation, resolution, colour space) so photos stay upright. Pass `-a` to drop them too.
- Keeps colour profiles so colours look the same.
- Replaces files atomically and keeps their permissions.

Not covered yet: HEIC, TIFF, RAW, PDF and video files, as well as file names, timestamps and extended attributes.

See `man ./metarm.1` for details.

## License

MIT
