# The ARX container format, version 1.0

All integers are little-endian. Strings are a `u32` byte length followed by UTF-8. An archive is a
header, a stream of records, an index record and a footer:

```
archive = header record* index-record footer
```

## Header (32 bytes)

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `ARX\0` |
| 4 | 2 | major version (1) |
| 6 | 2 | minor version (0) |
| 8 | 1 | default codec (0 none, 1 lz4, 2 deflate) |
| 9 | 1 | hash algorithm (1 = BLAKE3) |
| 10 | 2 | flags (0) |
| 12 | 4 | chunk size in bytes (the most a chunk may hold) |
| 16 | 8 | creation time, Unix seconds |
| 24 | 4 | reserved (0) |
| 28 | 4 | CRC-32 of bytes 0 to 27 |

A reader refuses an archive whose major version is greater than its own, with a message saying so.

## Record

```
record = rec-header(32) meta(meta_len) meta-crc(4) [payload(data_len) digest(32) payload-crc(4)]
```

The part in brackets exists only for `Chunk` records.

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | marker `ARXR` |
| 4 | 1 | kind (1 FileBegin, 2 Chunk, 3 FileEnd, 4 Dir, 5 Symlink, 6 Index) |
| 5 | 1 | codec of the payload (chunks) |
| 6 | 2 | reserved (0) |
| 8 | 4 | `meta_len` (at most 2^30) |
| 12 | 8 | `data_len`, bytes of stored payload (at most 2^32) |
| 20 | 8 | `raw_len`, bytes of the chunk once decompressed |
| 28 | 4 | CRC-32 of bytes 0 to 27 |

The header checksum is verified before any length in the header is used. `meta-crc` is the CRC-32 of
the meta bytes. For a chunk, `digest` is the BLAKE3 hash of the uncompressed bytes and `payload-crc`
the CRC-32 of the stored bytes. A chunk whose compressed form is not smaller than its raw form is
stored with codec 0.

## Meta layouts

| Kind | Fields |
|---|---|
| FileBegin | `id:u64` `path:str` `size:u64` `mtime_secs:i64` `mtime_nanos:u32` `mode:u32` |
| Chunk | `file_id:u64` `index:u64` `offset:u64` (offset of the chunk within the file) |
| FileEnd | `file_id:u64` `chunks:u64` `stored:u64` `size:u64` `digest:[32]` |
| Dir | `id:u64` `path:str` `mtime_secs:i64` `mtime_nanos:u32` `mode:u32` |
| Symlink | `id:u64` `path:str` `target:str` `mtime_secs:i64` `mtime_nanos:u32` `mode:u32` |
| Index | `count:u64`, then per entry: `kind:u8` `id:u64` `path:str` `size:u64` `mtime_secs:i64` `mtime_nanos:u32` `mode:u32` `offset:u64` `chunks:u64` `stored:u64` `digest:[32]` `target:str` |

Paths are relative, use `/`, and are UTF-8. `mode` holds Unix permission bits (`0444` stands for
read-only on Windows).

A file is `FileBegin`, then its chunks in order (indices 0, 1, 2, ... and offsets that add up), then
`FileEnd`. The file digest in `FileEnd` is BLAKE3 over the chunk digests in order. It is not the
BLAKE3 hash of the file, so it does not equal what `b3sum` prints.

## Footer (40 bytes)

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | marker `ARXE` |
| 4 | 8 | offset of the index record |
| 12 | 8 | length of the index record |
| 20 | 8 | number of entries |
| 28 | 8 | total uncompressed bytes of all files |
| 36 | 4 | CRC-32 of bytes 0 to 35 |

The index record sits just before the footer. With it a reader can list the archive and jump to any
file. Without it (a pipe, a truncated or damaged file) the archive can still be read from the front,
because every file's records carry its path and metadata.

## What each byte is protected by

| Bytes | Checked by |
|---|---|
| Header | its CRC-32 |
| Record header | its CRC-32 (and the marker) |
| Meta | the meta CRC-32 |
| Chunk stored bytes | the payload CRC-32 |
| Chunk contents | the BLAKE3 digest |
| Chunk digest field | the comparison with the hash of the decompressed data, and the chain digest in `FileEnd` |
| Order and completeness of a file's chunks | the chunk numbers, the offsets, the counts and the chain digest |
| Index and footer | their CRC-32s, and the comparison of the index with the records read |

These checksums find accidental damage. They are not a signature: anyone who edits an archive can
recompute them.

## Recovering after damage

If a record header fails its checksum the reader cannot tell where the next record starts. With
recovery on, it scans forward for the marker `ARXR` followed by a header whose checksum is valid, and
continues from there. A record whose meta fails its checksum but whose header is intact has a known
extent and is stepped over.

## Extraction rules

A stored path is refused if it is empty, absolute, contains `..` or `.` components, a backslash, a
colon, a control character, one of `< > " | ? *`, a Windows device name, or a component ending in a dot
or a space. A path is never created through a symbolic link that is already in the destination, and
`--force` replaces a symbolic link in the way instead of writing through it.
