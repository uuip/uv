# Downloading dependencies into a wheelhouse

`uv download` resolves a project's `uv.lock` for a target platform and writes every wheel (and any
sdists referenced by the lockfile) into an output directory, **without** creating a virtual
environment or installing anything. Use it to pre-populate an offline wheelhouse, build a container
image in multiple layers, or hand off a frozen artifact set to another environment.

## Quickstart

```console
$ uv download -o pkgs
Resolved 25 packages in 142ms
Downloaded 25 packages (0 already existed) to pkgs
```

The command reads `pyproject.toml` and `uv.lock` from the current project. Passing `-o/--output-dir`
is the only required flag; everything else defaults to the host environment.

## Cross-platform target

To populate a wheelhouse for an aarch64 Linux container from a developer laptop:

```console
$ uv download \
    --platform linux \
    --machine aarch64 \
    --glibc 2.28 \
    -o aarch64-wheels
```

The target must evaluate against one of the lockfile's `tool.uv.environments` markers, matching the
check performed by `uv sync`. Otherwise, `uv download` exits with an error pointing at the relevant
environment configuration.

## Flags

| Flag                 | Default             | Accepts                                                                                                                            |
| -------------------- | ------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `--platform`         | host OS             | `linux`, `windows` (alias `win32`), `macos` (aliases `darwin`, `osx`)                                                              |
| `--machine`          | host arch           | `x86_64`, `aarch64`, `i686` (Windows only), `riscv64` (Linux only); aliases incl. `amd64`, `x86-64`, `x64`, `arm64`, `i386`, `x86` |
| `--glibc`            | `2.28` (Linux only) | `MAJOR.MINOR` (e.g. `2.28`) or `MAJOR_MINOR` (e.g. `2_28`)                                                                         |
| `--implementation`   | `CPython`           | `CPython` only (other implementations deferred)                                                                                    |
| `-o`, `--output-dir` | required            | any directory, created if missing                                                                                                  |

All values are case-insensitive. `--glibc` is rejected for non-Linux targets.

Standard project-wide flags are supported: `--extra`, `--all-extras`, `--group`, `--only-group`,
`--no-default-groups`, `--dev`/`--no-dev`, `--locked`, `--frozen`, `--refresh`, plus the usual
index, keyring, and build options.

## Interaction with Python interpreters

`uv download` uses an interpreter only to compute markers and tags — no venv is created. If you pass
`--python 3.14` and the host does not have 3.14 installed, uv will fetch a managed Python build
automatically (same mechanism as `uv sync --python 3.14`). Pass `--no-python-downloads` to disable
that and require a local interpreter.

## Output layout

Wheels and sdists land directly under `--output-dir`, one file per distribution, using the original
distribution filename as published on the index. Re-running against the same directory skips files
that already exist:

```console
$ uv download -o pkgs
Downloaded 25 packages (0 already existed) to pkgs
$ uv download -o pkgs
Downloaded 0 packages (25 already existed) to pkgs
```

## Skipped dependencies

Dependencies that cannot be materialized into a portable artifact are omitted from the wheelhouse:

- The current project and virtual workspace roots — silently omitted (they would need to be built).
- Other workspace members — skipped with a warning.
- Local `path` sources — skipped with a warning (buildable but not a stable binary artifact).
- `editable = true` dependencies — skipped with a warning.
- `git` sources — skipped with a warning.

Because the root project is omitted silently, the summary line typically reports one fewer
`Downloaded` package than `Resolved` (or more, when workspace members are also present).

The remaining resolved distributions are downloaded as `.whl` or `.tar.gz` files directly from the
index, without any extraction or re-archiving. Registry and direct-URL artifacts are streamed
byte-for-byte from the upstream URL and, when `uv.lock` records hashes for them, their SHA-256 is
verified on the way in. That means the output matches what was published on the index, so downstream
tools such as `pip install --require-hashes` will accept it. Local `path` wheels are copied or
hard-linked from disk; their bytes are whatever you point the dependency at.

## Rewriting PyPI artifact URLs to a mirror

When `--default-index` points at a non-PyPI mirror, `uv download` rewrites
`https://files.pythonhosted.org/packages/...` URLs recorded in the lockfile to
`{mirror}/packages/...` **in memory only**. `uv.lock` on disk is never modified, and the SHA-256
hash recorded for each file is still verified against the downloaded bytes — a misconfigured mirror
surfaces as a hash mismatch rather than a silent substitution.

```console
$ uv lock                                                    # resolves against PyPI as usual
$ uv download -o pkgs \
    --default-index https://pypi.tuna.tsinghua.edu.cn/simple/
Downloaded 25 packages (0 already existed) to pkgs
```

The rewrite is deliberately narrow:

- It applies only to URLs whose host is `files.pythonhosted.org`. URLs from corporate or custom
  indexes with their own file layout are left untouched — rewriting them to `{mirror}/packages/...`
  would silently 404.
- It is skipped when `--default-index` itself points at PyPI.
- If `--default-index` points at a local path, or at a URL whose final segment is not `simple` /
  `+simple`, `uv download` emits a warning and falls back to the URLs recorded in `uv.lock`.

The rewrite is a **download-time** transformation. It never changes the lockfile, and it never
participates in lock resolution. With `--locked` or `--frozen`, the lock check runs against the
original URLs as recorded; only the subsequent HTTP requests go to the mirror. This means:

- `uv download --locked --default-index <mirror>` still passes the strict lock consistency check,
  because the check is performed against the URLs in `uv.lock`, not the rewritten ones.
- `uv download --frozen --default-index <mirror>` is the typical mirror use case — lock is trusted,
  and every fetch goes to the mirror.

## See also

- [`uv sync`](../concepts/projects/sync.md) — creates or updates a `.venv`.
- [`uv lock`](../concepts/projects/sync.md#checking-the-status-of-the-lockfile) — updates the
  lockfile without installing or downloading.
- [`tool.uv.environments` and `required-environments`](../concepts/projects/config.md) — tell uv
  which platforms a lockfile must cover.
