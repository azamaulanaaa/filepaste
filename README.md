# filepaste

filepaste is a self-hosted file paste service. A client uploads a file
with an HTTP PUT request. The service stores the file and returns a link.
A client downloads the file with the link.

## Requirements

- Rust 1.91 or newer. `Cargo.toml` sets `rust-version = "1.91"` and
  edition 2024.
- Cargo.
- `make` for the `Makefile` targets.
- `mise` for the development tools that the checks use.

Install the development tools:

```bash
mise install
```

## Build and run

Build the release binary:

```bash
cargo build --release
```

Run the service:

```bash
cargo run --release
```

The binary is `target/release/filepaste`. The first start writes a
default configuration. The service listens on `0.0.0.0:3000`. It prints
a TOTP URL and a QR code. Add the TOTP entry to an authenticator app.
The account name is `default` and the issuer is `filepaste`.

The default storage root is `./data`. The service creates the directory
when it does not exist.

## Configuration

The service loads its configuration with `confy`. The application name
is `filepaste` and the config name is `default-config`. The file is
`$XDG_CONFIG_HOME/filepaste/default-config.toml`. The default path is
`~/.config/filepaste/default-config.toml`.

The service creates the file with these defaults when the file is
absent:

```toml
default_retention_hours = 24
password_salt = "<32 random alphanumeric characters>"
totp_secret = "<32 random alphanumeric characters>"

[endpoint]
host = "0.0.0.0"
port = 3000

[storage]
type = "local"
root = "./data"
```

| Key | Default | Function |
| --- | --- | --- |
| `endpoint.host` | `0.0.0.0` | Bind address. |
| `endpoint.port` | `3000` | Bind port. |
| `endpoint.index_path` | unset | Path to an index file. |
| `storage.type` | `local` | Storage type: `local` or `s3`. |
| `storage.root` | `./data` | Storage root for the `local` type. |
| `storage.bucket` | unset | S3 bucket name for the `s3` type. |
| `storage.region` | unset | S3 region for the `s3` type. |
| `storage.key_prefix` | empty | Key prefix for every S3 object. |
| `storage.endpoint_url` | unset | Endpoint of an S3-compatible service. |
| `default_retention_hours` | `24` | Retention time. |
| `password_salt` | random | Salt for key derivation. |
| `totp_secret` | random | TOTP secret. |

The `s3` storage type stores file content in an S3 bucket. Set
`storage.bucket` and `storage.region`. The `storage.key_prefix` value is
optional. Set `storage.endpoint_url` to use an S3-compatible service.
The service uses path-style access when `storage.endpoint_url` is set.

The S3 provider reads credentials from the environment. Set
`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`. Set `AWS_SESSION_TOKEN`
when the credentials include a session token. The service reads the
credentials at request time.

The `password_salt` and `totp_secret` values are 32 random
alphanumeric characters. The service creates new values when it writes
a new file. Keep the file. A new `password_salt` makes the stored files
unreadable. A new `totp_secret` invalidates the TOTP entry.

## Command-line options

| Option | Function |
| --- | --- |
| `-v`, `--verbose` | Increase the log level. |
| `-c`, `--config <path>` | Accepted, but not used. |

The default log level is INFO. One `-v` sets DEBUG. Two or more `-v`
set TRACE.

## HTTP API

The service has four routes:

| Method | Path | Auth | Response |
| --- | --- | --- | --- |
| `PUT` | `/{filename}` | TOTP and password | The file URL. |
| `POST` | `/api/remote-upload` | TOTP and password | The file URL. |
| `GET` | `/{random}/{filename}` | Password | The file. |
| `GET` | `/` | None | The index page. |

### Authentication

The service uses HTTP Basic auth. For an upload, the user name is the
current TOTP code and the password is the file password. For a
download, the service uses the password to decrypt the file. The user
name has no effect on a download.

The service encrypts each file with ChaCha20-Poly1305. It derives the
key from the password and `password_salt` with PBKDF2-HMAC-SHA256 and
600000 iterations. The same password is necessary to download the
file.

### Upload

Send the file with `PUT`:

```bash
curl -u "$AUTH" \
  -X PUT --data-binary @report.pdf \
  http://127.0.0.1:3000/report.pdf
```

`AUTH` holds the TOTP code and the file password in the form
`<totp>:<password>`. `PUT` accepts one path segment. A sub-path returns
400. An absolute path and a path with `..` that leaves the root return
400. A wrong TOTP code returns 401 with `Invalid TOTP`. A request with
no Basic auth returns 401.

The response is the file URL:

```text
http://127.0.0.1:3000/Ab3dE9fG/cmVwb3J0LnBkZg
```

The first segment is 8 random alphanumeric characters. The second
segment is the file name in base64url without padding.

### Remote upload

Send a curl command to `POST /api/remote-upload`. The service runs the
command, stores the response body, and returns the file URL. Use `AUTH`
in the same form as `PUT`.

Send the command as plain text:

```bash
curl -u "$AUTH" \
  -H 'Content-Type: text/plain' \
  --data-binary 'curl -L https://example.com/report.pdf' \
  http://127.0.0.1:3000/api/remote-upload
```

Or send a JSON object with a `command` field:

```bash
curl -u "$AUTH" \
  -H 'Content-Type: application/json' \
  --data '{"command":"curl https://example.com/report.pdf"}' \
  http://127.0.0.1:3000/api/remote-upload
```

The service accepts one URL with an `http` or `https` scheme. It accepts
the `-X`, `-H`, `-d`, `-u`, `-L`, `--max-time`, and `-o` options. It
rejects other options with 400. It accepts `-L`, but it does not follow
redirects. It rejects a target with a private, loopback, link-local, or
reserved address with 403. The command limit is 8 KiB. The response body
limit is 16 MiB. The default fetch timeout is 30 seconds. The
`--max-time` option sets the timeout up to 60 seconds.

### Download

Send the password as the Basic auth password:

```bash
curl -u ":$PASSWORD" -OJ \
  http://127.0.0.1:3000/Ab3dE9fG/cmVwb3J0LnBkZg
```

`PASSWORD` is the file password. The response is
`application/octet-stream` with the original file name in
`Content-Disposition`. A missing file returns 404. A bad path returns
400.

### Index page

`GET /` returns the embedded `static/index.html`. The page uploads one
file. Set `endpoint.index_path` to a file path to serve another index
page. The service uses the embedded page when it cannot open that file.

### Retention

Each file has a retention time. The default is `default_retention_hours`
(24 hours). Send the `X-Retention-Hour` header on upload to set another
time in hours. The service locks the file until the time expires. A
cleanup task runs every hour. It deletes the files that are past their
retention time.

## Checks

Run every check:

```bash
make ci
```

`make ci` runs `scripts/ci.sh`. The script runs each `scripts/check-*.sh`
in name order. The checks include the Rust test suite, clippy, rustfmt,
markdownlint, shellcheck, and other repository checks. The
[check pipeline](docs/ci.md) describes the checks.

The S3 round-trip test is ignored by default. It needs a real bucket.
Set `FILEPASTE_S3_BUCKET` and `FILEPASTE_S3_REGION` to run it:

```bash
cargo test --test s3_storage -- --ignored
```

Run the checks in a smolvm microVM:

```bash
make smolvm
```

`make smolvm` runs the same checks in a smolvm machine. In local mode,
the machine uses an offline pack and a read-only mount of the
repository. In remote mode, `SMOLVM_ADDR` selects a `smolvm serve`
server, and the script uploads a copy of the working tree. The remote
machine has network access. These variables change the run:

| Variable | Default | Function |
| --- | --- | --- |
| `SMOLVM_ADDR` | unset | Address of a `smolvm serve` server. |
| `SMOLVM_OWNER` | path hash | Owner name. Required in remote mode. |
| `SMOLVM_CPUS` | `1` | CPU count. |
| `SMOLVM_MEMORY_MB` | `1024` | Memory in MiB. |

Set `SMOLVM_ADDR` to run the checks on a remote server. Set
`SMOLVM_OWNER` to a unique name for the repository. The
[smolvm document](docs/smolvm.md) describes the local and remote flows.

Run one command in the microVM:

```bash
make smolvm-exec ARGS='df -h /'
```

## Release

Run the release on `main`:

```bash
make release
```

The target runs `scripts/release.sh`. The script runs `cog bump` with
`--auto` by default. It updates `CHANGELOG.md`, creates a version
commit, and creates a tag with the `v` prefix. The tag stays local. Push
the tag to start the optional GitHub Release workflow.

The script uses a smolvm sandbox when it can. The guest clones the
repository to a writable directory and runs the bump there. The guest
returns the changelog, the commit, and the tag as a git bundle. The
host checks the bundle and applies it. `SMOLVM_ENABLED=0` forces the
host path. `SMOLVM_ENABLED=1` requires the sandbox. Set `SMOLVM_ADDR`
to use a remote server.

Show the next version and change nothing:

```bash
make release ARGS='--dry-run'
```

The [release workflow](docs/release.md) describes the checks and the
GitHub Release step.

## License

filepaste uses the MIT License. See [LICENSE](LICENSE).
