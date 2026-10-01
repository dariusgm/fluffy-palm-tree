# Installation (Ubuntu 24.04)

Ubuntu is the only supported OS.

## 1. System packages

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config libssl-dev cmake \
  ffmpeg \
  poppler-utils \
  cifs-utils
```

| Package | Needed for |
|---|---|
| `build-essential`, `pkg-config`, `cmake` | Compiling the bundled DuckDB and other native crates |
| `libssl-dev` | TLS support for the HTTP client |
| `ffmpeg` | `ffprobe` (video metadata) and `ffmpeg` (frame extraction) |
| `poppler-utils` | `pdftotext` / `pdfinfo` (PDF text extraction and page count) |
| `cifs-utils` | Mounting SMB network shares |

Check that the tools are on the `PATH`:

```bash
ffprobe -version | head -1
ffmpeg -version | head -1
pdftotext -v
```

## 2. Rust toolchain

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup component add clippy rustfmt
```

The first build compiles DuckDB from source, which takes a few minutes.

On first start the service runs `INSTALL fts`, which downloads DuckDB's full-text-search
extension once into `~/.duckdb/extensions`. If the machine has no internet access,
text search falls back to slower substring matching.

## 3. Mount the SMB shares

The service only reads local paths, so mount every share with CIFS and add the
mount point as a `[[roots]]` entry in `config.toml`.

Store the credentials in a file that stays out of the repository:

```bash
sudo install -m 600 /dev/null /etc/smb-credentials
sudo tee /etc/smb-credentials >/dev/null <<'EOF'
username=YOUR_USER
password=YOUR_PASSWORD
EOF
```

Add a read-only mount to `/etc/fstab`:

```
//NAS_HOST/SHARE  /mnt/share/NAME  cifs  ro,credentials=/etc/smb-credentials,uid=YOUR_UID,gid=YOUR_GID,file_mode=0644,dir_mode=0755,iocharset=utf8,_netdev,nofail  0  0
```

```bash
sudo mkdir -p /mnt/share/NAME
sudo mount -a
```

Notes:
- `ro` guarantees the service can never modify the source data.
- On CIFS mounts, the stored unix permissions are the ones the mount reports
  (`file_mode`/`dir_mode`, or the server's unix extensions if enabled), not
  necessarily the real ACLs on the NAS.

## 4. Configure

```bash
cp config.example.toml config.toml   # config.toml is gitignored
$EDITOR config.toml
```

Set at least:
- `[[roots]]`: one entry per mounted share
- `[llm].base_url` and `[llm].model`. You can also set
  `MEDIA_SEARCH_LLM_URL` in the environment instead of the config file.

## 5. llama.cpp server (on the LLM machine)

Image and video analysis need a **vision-capable** model. `llama-server` must be
started with the model's multimodal projector:

```bash
llama-server -m MODEL.gguf --mmproj MMPROJ.gguf --host 0.0.0.0 --port 8080 -c 16384
```

Check it from the search host with `curl http://LLM_HOST:8080/v1/models`.

## 6. Build and run

```bash
cargo build --release
./target/release/media-search            # reads ./config.toml
MEDIA_SEARCH_CONFIG=/etc/media-search.toml ./target/release/media-search
RUST_LOG=debug ./target/release/media-search
```
