# wallpaper-downloader

A command-line tool to download wallpapers from [Basic Apple Guy](https://basicappleguy.com) and organize them by device
type.

## Building

Requires [Rust](https://rustup.rs/) (1.70+).

```sh
cargo build --release
```

The binary will be at `target/release/wallpaper-downloader`.

## Usage

```sh
# Download with defaults (saves to ./downloads/, sorted by device and article)
cargo run

# Custom output directory
cargo run -- -o ~/Wallpapers

# Only sort by device, no article subfolders
cargo run -- --device-sort true --article-sort false

# Only sort by article, no device subfolders
cargo run -- --device-sort false --article-sort true

# Flat output (no sorting at all)
cargo run -- --device-sort false --article-sort false

# Slow and sequential (useful for debugging or being extra polite)
cargo run -- -j 1 -i 1 --delay 500
```

## Options

| Flag                  | Short | Default     | Description                                                        |
| --------------------- | ----- | ----------- | ------------------------------------------------------------------ |
| `--output`            | `-o`  | `downloads` | Output directory for downloaded wallpapers                         |
| `--parallel-articles` | `-j`  | `4`         | Max concurrent article fetches                                     |
| `--parallel-images`   | `-i`  | `3`         | Max concurrent image downloads per article                         |
| `--device-sort`       |       | `true`      | Sort images into device subfolders (iPhone/, iPad/, Mac/, Others/) |
| `--article-sort`      |       | `true`      | Sort images into article subfolders                                |
| `--delay`             |       | `250`       | Delay in milliseconds between downloads                            |
| `--open`              |       | `false`     | Open the output folder when finished                               |
| `--config`            | `-c`  | (see below) | Path to a TOML config file                                         |
| `--version`           | `-V`  |             | Print version                                                      |

## Configuration

Settings can be persisted in a TOML config file so you don't have to pass flags every time. CLI flags always take
precedence over the config file.

The config file is looked up in this order:

1. `--config /path/to/config.toml` (explicit)
2. `./config.toml` (current directory)
3. `~/.config/wallpaper-downloader/config.toml` (global fallback)

An example `config.toml` is provided in the project root.

### Example config

```toml
output = "~/Wallpapers"
parallel_articles = 2
parallel_images = 4
device_sort = true
article_sort = true
delay = 300
open_on_finish = false
```

All fields are optional — only include the ones you want to override.

## Behavior

- **Skip existing:** Images that already exist on disk are skipped without re-downloading. Re-running the tool is fast
  and safe.
- **Retry on failure:** Failed image downloads are automatically retried once after all other images in the article
  complete.
- **Resilient pagination:** If a category page fails to load mid-pagination, the tool continues with the articles found
  so far rather than aborting.

## Output Structure

With both sorts enabled (default):

```text
downloads/
  iPhone/
    floral/
      BAG-iPhoneWallpaper.jpg
    hive/
      BAG-iPhoneWallpaper.jpg
  iPad/
    floral/
      BAG-iPadWallpaper.jpg
  Mac/
    floral/
      BAG-MacDesktop.jpg
```

With only `device_sort`:

```text
downloads/
  iPhone/
    BAG-iPhoneWallpaper.jpg
  Mac/
    BAG-MacDesktop.jpg
```

With only `article_sort`:

```text
downloads/
  floral/
    BAG-iPhoneWallpaper.jpg
    BAG-MacDesktop.jpg
```

With neither:

```text
downloads/
  BAG-iPhoneWallpaper.jpg
  BAG-MacDesktop.jpg
```
