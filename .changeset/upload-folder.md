---
default: patch
---

# Upload files by path from a shared folder

- The NixOS module creates `STATE_DIR/uploads`, writable by every local user
  and cleared of files older than a day. Agents save a file there and pass its
  `path` to `upload_file`, so its bytes never pass through the model.
- `upload_file`'s description names that folder when the server has one, and
  steers agents to `path` over base64 `content`.
- AVIF, HEIC, HEIF, BMP and TIFF images get their own MIME type.
