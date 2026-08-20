# Paper frame server

A small Rust HTTP server that provides one JPEG to a PaperlessPaper frame running the offline firmware. It is intended for a trusted home LAN.

The server exposes:

- `GET /settings` — a minimal status page showing the latest image GET.
- `GET /frame/current.jpg` — the configured JPEG.
- `HEAD /frame/current.jpg` — image metadata without changing the latest-GET status.

## Requirements

- A current stable Rust toolchain
- A JPEG to display
- Network access between the frame and this server

## Configure and run

Copy the example configuration and prepare the image directory:

```sh
cp config.example.toml config.toml
mkdir -p data
cp /path/to/photo.jpg data/current.jpg
```

`config.toml` contains:

```toml
bind = "0.0.0.0:8080"
image_path = "data/current.jpg"
```

The bind value is a socket address. Relative image paths are resolved from the process working directory. Start the server with:

```sh
cargo run --release -- config.toml
```

The configuration argument is optional and defaults to `config.toml`. Set `RUST_LOG` to adjust structured logging, for example `RUST_LOG=debug`.

Configure the frame's image URL as:

```text
http://SERVER_LAN_IP:8080/frame/current.jpg
```

Open the status page at `http://SERVER_LAN_IP:8080/settings`.

## Check the endpoints

Download the exact configured bytes and show response headers:

```sh
curl --dump-header /tmp/paperframe-headers \
  --output /tmp/paperframe-current.jpg \
  http://127.0.0.1:8080/frame/current.jpg
cat /tmp/paperframe-headers
cmp data/current.jpg /tmp/paperframe-current.jpg
```

Check metadata without changing the latest GET shown on the settings page:

```sh
curl --head http://127.0.0.1:8080/frame/current.jpg
```

Test a conditional GET using the returned `Last-Modified` value:

```sh
last_modified=$(curl --silent --head \
  http://127.0.0.1:8080/frame/current.jpg |
  awk 'BEGIN { IGNORECASE=1 } /^Last-Modified:/ { sub(/^[^:]*: /, ""); sub(/\r$/, ""); print }')
curl --include --header "If-Modified-Since: $last_modified" \
  http://127.0.0.1:8080/frame/current.jpg
```

An unchanged image returns `304 Not Modified` with no body. A normal image GET, including a `304`, updates the status page. A HEAD request does not.

## Replace the current image

The file is opened on every request, so no server restart is needed. Replace it atomically to prevent the frame from seeing a partially written JPEG:

```sh
cp /path/to/new-photo.jpg data/current.jpg.tmp
mv data/current.jpg.tmp data/current.jpg
```

Create the temporary file on the same filesystem so `mv` is atomic. `Last-Modified` has one-second HTTP precision; avoid replacing an image more than once in the same second when relying on conditional requests. The frame's force-download gesture can bypass its cache during testing.

The server labels the configured bytes as `image/jpeg` but does not validate or transform them. Supply a valid JPEG supported by the frame firmware.

## Status semantics and limitations

The status page stores the UTC time, direct TCP peer IP, and selected HTTP response status of the latest image GET. `200`, `304`, `404`, and server-error requests are recorded. The state is in memory and resets whenever the process restarts.

The peer IP is taken from the TCP connection. Forwarding headers such as `X-Forwarded-For` are deliberately ignored, so deployment behind a reverse proxy would show the proxy's address. For reliable frame identification, connect it directly and consider a DHCP reservation.

A recorded successful GET means the server handled the request. It does **not** prove that the client received every byte, decoded the JPEG, or refreshed the physical e-ink panel.

## Security boundary

This initial server has no authentication, authorization, or TLS. Anyone who can reach it can fetch the current image and inspect the status page. Bind it only to a trusted home LAN and restrict access with host/router firewall rules. Do not expose it directly to the Internet.

## Development checks

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```
