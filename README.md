# metarm
> a metadata removal tool

Removes metadata (EXIF, GPS, XMP, IPTC, comments, text chunks) from images, invoked like `rm`:

```sh
metarm photo.jpg            # strip one file
metarm a.jpg b.png c.webp   # several files
metarm -rf ~/Pictures/out   # every supported file in a directory, recursively
metarm -a photo.jpg         # remove everything, including orientation
```

By default it keeps only the tags that affect how an image is displayed: orientation (so photos stay upright), resolution and colour space. They go into a freshly built EXIF block, so nothing else survives. Pass `-a` to drop those too.

Supports JPEG, PNG/APNG and WebP, detected by content rather than extension. Pixel data is never re-encoded, and files are replaced atomically.

Build with `cargo build --release`. See the man page (`man ./metarm.1`) for all options.

## Install

```sh
cargo install metarm
```

## License

MIT
