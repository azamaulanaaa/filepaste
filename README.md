# filepaste

filepaste is a self-hosted file paste service. A client uploads a file
with an HTTP PUT request. The service stores the file and returns a link.
A client downloads the file with the link.

## Requirements

- Rust 1.94.1 or newer. `Cargo.toml` sets `rust-version = "1.94.1"` and
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
job_store_path = "/home/<user>/.local/state/filepaste/jobs"

[storage]
type = "local"
root = "./data"
```

| Key | Default | Function |
| --- | --- | --- |
| `endpoint.host` | `0.0.0.0` | Bind address. |
| `endpoint.port` | `3000` | Bind port. |
| `endpoint.index_path` | unset | Path to an index file. |
| `endpoint.job_store_path` | state directory | Job record directory. |
| `storage.type` | `local` | Storage type: `local` or `s3`. |
| `storage.root` | `./data` | Storage root for the `local` type. |
| `storage.bucket` | unset | S3 bucket name for the `s3` type. |
| `storage.region` | unset | S3 region for the `s3` type. |
| `storage.key_prefix` | empty | Key prefix for every S3 object. |
| `storage.endpoint_url` | unset | Endpoint of an S3-compatible service. |
| `storage.multipart_upload_expiry_hours` | `24` | Incomplete upload expiry. |
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

The provider uploads an object in parts of 8 MiB and keeps at most one
part in memory. S3 accepts at most 10000 parts, so the largest object is
about 78 GiB.

The periodic GC run aborts incomplete multipart uploads that are older
than `storage.multipart_upload_expiry_hours`. The default value is 24
hours. A positive value is at least 1 hour. Set the value to `0` to
disable cleanup. The cleanup needs the `s3:ListBucketMultipartUploads`
and `s3:AbortMultipartUpload` permissions.

A non-empty `storage.key_prefix` gives strict isolation because the
cleanup aborts only uploads under `<prefix>/`. With an empty prefix, the
cleanup accepts only keys with the service shape: 8 ASCII alphanumeric
characters, a slash, and a Base64Url filename. The cleanup logs skipped
foreign keys at debug level.

The `password_salt` and `totp_secret` values are 32 random
alphanumeric characters. The service creates new values when it writes
a new file. Keep the file. A new `password_salt` makes the stored files
unreadable. A new `totp_secret` invalidates the TOTP entry.

The service writes the remote upload job records to
`endpoint.job_store_path`. When `XDG_STATE_HOME` names an absolute
path, the default is `$XDG_STATE_HOME/filepaste/jobs`. Otherwise the
default is `$HOME/.local/state/filepaste/jobs`. The directory is not
inside the storage root. The service creates the directory with mode
`0700` and writes each record file with mode `0600` on Unix. It logs a
warning and skips a corrupt record.

## Command-line options

| Option | Function |
| --- | --- |
| `-v`, `--verbose` | Increase the log level. |
| `-c`, `--config <path>` | Accepted, but not used. |

The default log level is INFO. One `-v` sets DEBUG. Two or more `-v`
set TRACE.

## HTTP API

The service has seven routes:

| Method | Path | Auth | Response |
| --- | --- | --- | --- |
| `PUT` | `/{filename}` | TOTP and password | The file URL. |
| `POST` | `/api/remote-upload` | TOTP and password | The file URL. |
| `POST` | `/api/remote-uploads` | TOTP and password | Job ID and token. |
| `GET` | `/api/remote-uploads/{id}` | Bearer job token | The job status. |
| `DELETE` | `/api/remote-uploads/{id}` | Bearer job token | 204. |
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

### Remote upload jobs

The job API reports the progress of a remote upload. Send the same curl
command to `POST /api/remote-uploads`:

```bash
curl -u "$AUTH" \
  -H 'Content-Type: text/plain' \
  --data-binary 'curl -L https://example.com/report.pdf' \
  http://127.0.0.1:3000/api/remote-uploads
```

The service validates the command and the target address before it
returns 202. It does not wait for the target response. The response has
the job ID, the job token, the state, and the received byte count:

```json
{
  "id": "6f3a9b2c1d4e5f60718293a4b5",
  "token": "<job-token>",
  "state": "fetching",
  "received_bytes": 0
}
```

The service returns the token only in this response. Store it in memory.
Send it as a Bearer token to read the status:

```bash
curl -H "Authorization: Bearer $JOB_TOKEN" \
  http://127.0.0.1:3000/api/remote-uploads/$JOB_ID
```

The status has the state `fetching`, `done`, or `failed`. A `done` status
has the file URL. A `failed` status has an error message. A later status
has `total_bytes` when the target sends `Content-Length`. A target
failure appears as `state: "failed"` with an `error` field. It is not an
HTTP 502 after the job starts. The service sets `Retry-After: 1` while
the state is `fetching`. It sets `Cache-Control: no-store` on every
response.

The service keeps a finished job for one hour. An expired job returns
404. The service writes each job record to `endpoint.job_store_path`.
A restart keeps a finished job. A job that the restart interrupted
becomes `failed` with the error `Server restarted before the job
completed`. The stored file stays after a restart when the fetch
completed.

Cancel a job with `DELETE`:

```bash
curl -X DELETE -H "Authorization: Bearer $JOB_TOKEN" \
  http://127.0.0.1:3000/api/remote-uploads/$JOB_ID
```

The service stops the fetch and deletes a partial file. It keeps a
completed file. The request returns 204. An unknown job returns 404. A
missing or bad token returns 401.

The old `POST /api/remote-upload` endpoint stays for compatibility. It
waits for the fetch and returns the file URL as plain text.

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
local file. The page also accepts a curl command or a URL. It sends the
command to `POST /api/remote-uploads` and polls the job status. The page
shows the received bytes and a percentage when the target size is known.
The remote form uses the same TOTP Basic auth and the
`X-Retention-Hour` header as the local upload. Set
`endpoint.index_path` to a file path to serve another index page. The
service uses the embedded page when it cannot open that file.

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
| `SMOLVM_MEMORY_MB` | `2048` | Memory in MiB. |

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
the tag to start the Release workflow. The workflow uses cargo-dist to
build the release assets and creates the GitHub Release. The container
workflow downloads the Linux assets and pushes the container images. It
updates `:latest` only for the newest release.

The release builds the Linux assets
`filepaste-x86_64-unknown-linux-gnu.tar.xz` and
`filepaste-aarch64-unknown-linux-gnu.tar.xz`.

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

The [release workflow](docs/release.md) describes the release and the
container image.

## License

filepaste uses the MIT License. See [LICENSE](LICENSE).
