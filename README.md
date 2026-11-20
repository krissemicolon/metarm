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
metarm -i photo.jpg         # list the metadata, change nothing
```

`-i` shows each piece of metadata and whether stripping would keep or remove it:

```
$ metarm -i photo.jpg
photo.jpg: JPEG
  strip  EXIF     Make              Apple
  strip  EXIF     Model             iPhone 15 Pro
  keep   EXIF     Orientation       6 (rotated 90° CW)
  strip  EXIF     DateTimeOriginal  2024:06:01 18:22:10
  strip  EXIF     BodySerialNumber  F2LXK1234
  strip  GPS      Position          47.370094, 8.541806
  strip  XMP      dc:creator        Jane Doe
  strip  Comment  Comment           shot at grandma's house
  keep   ICC      Profile           sRGB built-in
```

| Flag | Meaning |
| --- | --- |
| `-r`, `-R`, `--recursive` | Process directories recursively. Non-image files inside are skipped. |
| `-f`, `--force` | Does nothing. Accepted so `metarm -rf` works like `rm -rf`. |
| `-a`, `--all` | Also remove orientation, resolution and colour space (kept by default). |
| `-i`, `--info` | List the metadata in each file and what would be stripped, without changing anything. Combine with `-a` to preview `-a`. |
| `-v`, `--verbose` | Print each file as it is processed. With `-i`, show long values in full. |
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
